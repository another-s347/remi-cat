use async_stream::stream;
use futures::{Stream, StreamExt};
use futures_timer::Delay;
use serde::Deserialize;
use std::future::Future;
use std::time::{Duration, Instant};

use remi_core::agent::Agent;
#[cfg(feature = "http-client")]
use remi_core::config::AgentConfig;
pub use remi_core::config::RateLimitRetryPolicy;
use remi_core::error::AgentError;
use remi_core::types::{ChatRequest, ChatResponseChunk, Role};
use remi_transport::HttpTransport;

// ── OpenAI wire types (SSE payload parsing) ──────────────────────────────────

#[derive(Debug, Deserialize)]
struct OAIChatCompletionChunk {
    #[serde(default)]
    choices: Vec<OAIChoice>,
    usage: Option<OAIUsage>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct OAIChoice {
    #[serde(default)]
    index: usize,
    delta: OAIDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAIDelta {
    role: Option<String>,
    content: Option<String>,
    /// Thinking/reasoning content from models like Kimi K2.5 or DeepSeek-R1.
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OAIToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct OAIToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<OAIFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OAIFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAIUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(alias = "output_tokens", default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

// ── OpenAIClient ──────────────────────────────────────────────────────────────

/// OpenAI-compatible streaming chat client, generic over [`HttpTransport`].
///
/// On native targets with `http-client` feature, use [`OpenAIClient::new()`]
/// which automatically uses [`ReqwestTransport`](crate::http::ReqwestTransport).
///
/// For other targets (e.g. `wasm32-wasip2`), use [`OpenAIClient::with_transport()`]
/// to inject a host-provided transport.
pub struct OpenAIClient<T: HttpTransport> {
    transport: T,
    base_url: String,
    api_key: String,
    model: String,
    rate_limit_retry: Option<RateLimitRetryPolicy>,
}

impl<T: HttpTransport> Clone for OpenAIClient<T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            rate_limit_retry: self.rate_limit_retry.clone(),
        }
    }
}

/// Convenience constructor for native targets (uses [`ReqwestTransport`]).
///
/// This preserves the original `OpenAIClient::new(api_key)` API so existing
/// code continues to work without changes.
#[cfg(feature = "http-client")]
impl OpenAIClient<remi_transport::ReqwestTransport> {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_transport(remi_transport::ReqwestTransport::new(), api_key)
    }

    pub fn from_config(config: &AgentConfig) -> Self {
        let mut client = Self::new(config.api_key.clone().unwrap_or_default());
        if let Some(url) = &config.base_url {
            client.base_url = url.clone();
        }
        if let Some(model) = &config.model {
            client.model = model.clone();
        }
        if let Some(policy) = &config.rate_limit_retry {
            client.rate_limit_retry = Some(policy.clone());
        }
        client
    }
}

impl<T: HttpTransport> OpenAIClient<T> {
    /// Create an OpenAI client with a custom [`HttpTransport`].
    ///
    /// Use this on `wasm32-wasip2` or other targets where reqwest is unavailable:
    ///
    /// ```ignore
    /// let transport = MyWasiHttpTransport::new();
    /// let client = OpenAIClient::with_transport(transport, "sk-...");
    /// ```
    pub fn with_transport(transport: T, api_key: impl Into<String>) -> Self {
        Self {
            transport,
            base_url: "https://api.openai.com/v1".into(),
            api_key: api_key.into(),
            model: "gpt-4o".into(),
            rate_limit_retry: None,
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_rate_limit_retry(mut self, policy: RateLimitRetryPolicy) -> Self {
        self.rate_limit_retry = Some(policy);
        self
    }

    pub fn with_default_rate_limit_retry(self) -> Self {
        self.with_rate_limit_retry(RateLimitRetryPolicy::default())
    }

    pub fn without_rate_limit_retry(mut self) -> Self {
        self.rate_limit_retry = None;
        self
    }
}

async fn read_response_body<S>(mut body_stream: S) -> String
where
    S: Stream<Item = Result<Vec<u8>, remi_transport::HttpTransportError>> + Unpin,
{
    let mut body_bytes = Vec::new();
    while let Some(chunk) = body_stream.next().await {
        if let Ok(bytes) = chunk {
            body_bytes.extend_from_slice(&bytes);
        }
    }
    String::from_utf8_lossy(&body_bytes).into_owned()
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn retry_after_delay(headers: &[(String, String)]) -> Option<Duration> {
    if let Some(value) = header_value(headers, "retry-after-ms") {
        if let Ok(ms) = value.trim().parse::<u64>() {
            return Some(Duration::from_millis(ms));
        }
    }
    header_value(headers, "retry-after")
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|secs| *secs >= 0.0)
        .map(Duration::from_secs_f64)
}

fn model_http_error(status: u16, body_text: &str, retries_used: usize) -> AgentError {
    if status == 429 && retries_used > 0 {
        return AgentError::model(format!(
            "HTTP {status} after {retries_used} retries: {body_text}"
        ));
    }
    AgentError::model(format!("HTTP {status}: {body_text}"))
}

fn chunk_has_ttft_signal(chunk: &OAIChatCompletionChunk) -> bool {
    chunk.choices.iter().any(|choice| {
        choice.delta.role.is_some()
            || choice
                .delta
                .content
                .as_ref()
                .is_some_and(|content| !content.is_empty())
            || choice
                .delta
                .reasoning_content
                .as_ref()
                .is_some_and(|content| !content.is_empty())
            || choice
                .delta
                .tool_calls
                .as_ref()
                .is_some_and(|tool_calls| !tool_calls.is_empty())
    })
}

impl<T: HttpTransport> Agent for OpenAIClient<T> {
    type Request = ChatRequest;
    type Response = ChatResponseChunk;
    type Error = AgentError;

    fn chat(
        &self,
        req: ChatRequest,
    ) -> impl Future<Output = Result<impl Stream<Item = ChatResponseChunk>, AgentError>> {
        let transport = self.transport.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let client_rate_limit_retry = self.rate_limit_retry.clone();

        async move {
            let mut req = req;
            if req.model.is_empty() {
                req.model = model;
            }
            let rate_limit_retry = req.rate_limit_retry.clone().or(client_rate_limit_retry);
            req.stream = true;
            req.stream_options = Some(remi_core::types::StreamOptions {
                include_usage: true,
            });

            let request_model = req.model.clone();
            let request_base_url = self.base_url.clone();

            let body = serde_json::to_vec(&req).map_err(|e| {
                AgentError::model(format!(
                    "failed to serialize chat request model={} base_url={} error={} debug={:?}",
                    request_model, request_base_url, e, e
                ))
            })?;

            let headers = vec![
                ("Authorization".into(), format!("Bearer {api_key}")),
                ("Content-Type".into(), "application/json".into()),
            ];

            let mut retries_used = 0usize;
            let (response, request_started_at) = loop {
                let request_started_at = Instant::now();
                let response = transport
                    .post_streaming(url.clone(), headers.clone(), body.clone())
                    .await
                    .map_err(|e| {
                        AgentError::model(format!(
                            "streaming request failed model={} base_url={} detail={} debug={:?}",
                            request_model, request_base_url, e, e
                        ))
                    })?;

                if (200..300).contains(&response.status) {
                    break (response, request_started_at);
                }

                let status = response.status;
                let retry_after = retry_after_delay(&response.headers);
                let body_text = read_response_body(response.body).await;

                if status == 429 {
                    if let Some(policy) = &rate_limit_retry {
                        if retries_used < policy.max_retries {
                            let delay = policy.delay_for_retry(retries_used, retry_after);
                            retries_used += 1;
                            if !delay.is_zero() {
                                Delay::new(delay).await;
                            }
                            continue;
                        }
                    }
                }

                return Err(AgentError::model(format!(
                    "{} model={} base_url={}",
                    model_http_error(status, &body_text, retries_used),
                    request_model,
                    request_base_url
                )));
            };

            // Parse SSE from the streaming body — transport-agnostic
            let lines = remi_transport::http::sse_lines(response.body);

            Ok(stream! {
                let mut lines = std::pin::pin!(lines);
                let mut ttft_logged = false;
                while let Some(line) = lines.next().await {
                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }
                    if let Some(data) = line
                        .strip_prefix("data: ")
                        .or_else(|| line.strip_prefix("data:"))
                    {
                        let data = data.trim_start();
                        if data == "[DONE]" {
                            // keep reading — some APIs send usage chunk after [DONE]
                            continue;
                        }
                        match serde_json::from_str::<OAIChatCompletionChunk>(data) {
                            Err(_) => continue,
                            Ok(chunk) => {
                                if !ttft_logged && chunk_has_ttft_signal(&chunk) {
                                    ttft_logged = true;
                                    tracing::info!(
                                        model = %request_model,
                                        base_url = %request_base_url,
                                        retries_used,
                                        ttft_ms = request_started_at.elapsed().as_millis(),
                                        "Model TTFT"
                                    );
                                }
                                if let Some(usage) = chunk.usage {
                                    yield ChatResponseChunk::Usage {
                                        prompt_tokens: usage.prompt_tokens,
                                        completion_tokens: usage.completion_tokens,
                                        total_tokens: usage.total_tokens,
                                    };
                                }
                                for choice in &chunk.choices {
                                    if let Some(rc) = &choice.delta.reasoning_content {
                                        if !rc.is_empty() {
                                            yield ChatResponseChunk::ReasoningDelta { content: rc.clone() };
                                        }
                                    }
                                    if let Some(content) = &choice.delta.content {
                                        if !content.is_empty() {
                                            let role = choice.delta.role.as_deref().map(|r| match r {
                                                "user" => Role::User,
                                                "system" => Role::System,
                                                "tool" => Role::Tool,
                                                _ => Role::Assistant,
                                            });
                                            yield ChatResponseChunk::Delta { content: content.clone(), role };
                                        }
                                    }
                                    if let Some(tool_calls) = &choice.delta.tool_calls {
                                        for tc in tool_calls {
                                            if let Some(func) = &tc.function {
                                                if let (Some(id), Some(name)) = (&tc.id, &func.name) {
                                                    yield ChatResponseChunk::ToolCallStart {
                                                        index: tc.index,
                                                        id: id.clone(),
                                                        name: name.clone(),
                                                    };
                                                }
                                                if let Some(args_delta) = &func.arguments {
                                                    if !args_delta.is_empty() {
                                                        yield ChatResponseChunk::ToolCallDelta {
                                                            index: tc.index,
                                                            arguments_delta: args_delta.clone(),
                                                        };
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                yield ChatResponseChunk::Done;
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use remi_core::types::{ChatRequest, Message};
    use remi_transport::{HttpStreamingResponse, HttpTransport, HttpTransportError};

    #[derive(Clone)]
    struct TestTransport {
        responses: Arc<Mutex<VecDeque<TestResponse>>>,
        call_count: Arc<Mutex<usize>>,
    }

    #[derive(Clone)]
    struct TestResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl TestTransport {
        fn new(responses: Vec<TestResponse>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(responses))),
                call_count: Arc::new(Mutex::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            *self.call_count.lock().expect("call_count lock poisoned")
        }
    }

    impl HttpTransport for TestTransport {
        fn post_streaming(
            &self,
            _url: String,
            _headers: Vec<(String, String)>,
            _body: Vec<u8>,
        ) -> impl Future<Output = Result<HttpStreamingResponse, HttpTransportError>> + Send
        {
            let response = self
                .responses
                .lock()
                .expect("responses lock poisoned")
                .pop_front()
                .expect("missing test response");
            let call_count = self.call_count.clone();

            async move {
                *call_count.lock().expect("call_count lock poisoned") += 1;
                Ok(HttpStreamingResponse {
                    status: response.status,
                    headers: response.headers,
                    body: Box::pin(futures::stream::iter(vec![Ok(response.body.into_bytes())])),
                })
            }
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: String::new(),
            messages: vec![Message::user("hello")],
            tools: None,
            temperature: None,
            max_tokens: None,
            stream: true,
            stream_options: None,
            metadata: None,
            rate_limit_retry: None,
            extra_body: serde_json::Map::new(),
        }
    }

    fn sse_ok_body() -> String {
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n".to_string()
    }

    #[test]
    fn respects_server_retry_after_when_enabled() {
        let policy = RateLimitRetryPolicy {
            initial_delay_ms: 500,
            max_delay_ms: 2_000,
            multiplier: 2.0,
            respect_retry_after: true,
            ..RateLimitRetryPolicy::default()
        };

        assert_eq!(
            policy.delay_for_retry(0, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn retries_429_then_succeeds() {
        let transport = TestTransport::new(vec![
            TestResponse {
                status: 429,
                headers: vec![("retry-after-ms".into(), "0".into())],
                body: "rate limited".into(),
            },
            TestResponse {
                status: 200,
                headers: Vec::new(),
                body: sse_ok_body(),
            },
        ]);

        let client = OpenAIClient::with_transport(transport.clone(), "test-key")
            .with_rate_limit_retry(RateLimitRetryPolicy {
                max_retries: 1,
                initial_delay_ms: 0,
                max_delay_ms: 0,
                multiplier: 2.0,
                respect_retry_after: true,
            });

        let result = futures::executor::block_on(async {
            let stream = client.chat(request()).await.expect("chat should succeed");
            futures::pin_mut!(stream);

            let mut chunks = Vec::new();
            while let Some(chunk) = stream.next().await {
                chunks.push(chunk);
            }
            chunks
        });

        assert!(
            matches!(result.first(), Some(ChatResponseChunk::Delta { content, .. }) if content == "hi")
        );
        assert!(matches!(result.last(), Some(ChatResponseChunk::Done)));
        assert_eq!(transport.call_count(), 2);
    }

    #[test]
    fn stops_after_retry_budget_is_exhausted() {
        let transport = TestTransport::new(vec![
            TestResponse {
                status: 429,
                headers: vec![("retry-after-ms".into(), "0".into())],
                body: "rate limited".into(),
            },
            TestResponse {
                status: 429,
                headers: vec![("retry-after-ms".into(), "0".into())],
                body: "still rate limited".into(),
            },
        ]);

        let client = OpenAIClient::with_transport(transport.clone(), "test-key")
            .with_rate_limit_retry(RateLimitRetryPolicy {
                max_retries: 1,
                initial_delay_ms: 0,
                max_delay_ms: 0,
                multiplier: 2.0,
                respect_retry_after: true,
            });

        let err = match futures::executor::block_on(client.chat(request())) {
            Ok(_) => panic!("chat should fail"),
            Err(err) => err,
        };

        assert_eq!(
            err.to_string(),
            "Model error: HTTP 429 after 1 retries: still rate limited"
        );
        assert_eq!(transport.call_count(), 2);
    }
}
