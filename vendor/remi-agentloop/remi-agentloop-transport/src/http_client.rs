use async_stream::stream;
use futures::{Stream, StreamExt};
use std::pin::Pin;

use crate::http::HttpTransport;
use crate::sse::decode_sse_data;
use remi_core::agent::Agent;
use remi_core::protocol::{ProtocolError, ProtocolEvent};
use remi_core::types::LoopInput;

/// HTTP SSE client — connects to a remote Agent service exposing the
/// standard remi protocol over Server-Sent Events.
///
/// Generic over [`HttpTransport`] so it works on native (reqwest), WASM
/// (WIT host imports), and browser (fetch API) targets — just like
/// [`OpenAIClient<T>`].
///
/// Implements [`Agent<LoopInput, ProtocolEvent, ProtocolError>`], making
/// remote agents fully composable with local ones.
///
/// # Example (native)
///
/// ```ignore
/// let client = HttpSseClient::new("http://localhost:8080/chat");
/// let stream = client.chat("Hello!".into()).await?;
/// ```
///
/// # Example (custom transport for WASM)
///
/// ```ignore
/// let client = HttpSseClient::with_transport(
///     WitHttpTransport,
///     "http://localhost:8080/chat",
/// );
/// ```
pub struct HttpSseClient<T: HttpTransport> {
    transport: T,
    endpoint: String,
    headers: Vec<(String, String)>,
}

impl<T: HttpTransport> Clone for HttpSseClient<T> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            endpoint: self.endpoint.clone(),
            headers: self.headers.clone(),
        }
    }
}

/// Convenience constructor for native targets (uses [`ReqwestTransport`]).
#[cfg(feature = "http-client")]
impl HttpSseClient<crate::http::ReqwestTransport> {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self::with_transport(crate::http::ReqwestTransport::new(), endpoint)
    }
}

impl<T: HttpTransport> HttpSseClient<T> {
    /// Create an SSE client with a custom [`HttpTransport`].
    ///
    /// Use this on `wasm32-wasip2` or other targets where reqwest is unavailable:
    ///
    /// ```ignore
    /// let client = HttpSseClient::with_transport(WitHttpTransport, "http://...");
    /// ```
    pub fn with_transport(transport: T, endpoint: impl Into<String>) -> Self {
        Self {
            transport,
            endpoint: endpoint.into(),
            headers: Vec::new(),
        }
    }

    /// Add a custom header.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Add a Bearer token authorization header.
    pub fn with_bearer_token(self, token: &str) -> Self {
        self.with_header("Authorization", format!("Bearer {token}"))
    }
}

impl<T: HttpTransport> Agent for HttpSseClient<T> {
    type Request = LoopInput;
    type Response = ProtocolEvent;
    type Error = ProtocolError;

    fn chat(
        &self,
        req: LoopInput,
    ) -> impl std::future::Future<Output = Result<impl Stream<Item = ProtocolEvent>, ProtocolError>>
    {
        let transport = self.transport.clone();
        let endpoint = self.endpoint.clone();
        let mut headers = self.headers.clone();

        async move {
            // Serialize LoopInput as JSON body
            let body = serde_json::to_vec(&req).map_err(|e| ProtocolError {
                code: "serialize_error".into(),
                message: e.to_string(),
            })?;

            headers.push(("Content-Type".into(), "application/json".into()));

            // Use the generic transport to POST
            let response = transport
                .post_streaming(endpoint, headers, body)
                .await
                .map_err(|e| ProtocolError {
                    code: "http_error".into(),
                    message: e.to_string(),
                })?;

            if response.status < 200 || response.status >= 300 {
                // Read the error body
                let mut body_bytes = Vec::new();
                let mut body_stream = response.body;
                while let Some(chunk) = body_stream.next().await {
                    if let Ok(bytes) = chunk {
                        body_bytes.extend_from_slice(&bytes);
                    }
                }
                let body_text = String::from_utf8_lossy(&body_bytes);
                return Err(ProtocolError {
                    code: "http_error".into(),
                    message: format!("HTTP {}: {}", response.status, body_text),
                });
            }

            // Parse SSE from the streaming body — transport-agnostic
            let lines = crate::http::sse_lines(response.body);

            Ok(stream! {
                let mut lines: Pin<Box<_>> = lines;
                while let Some(line) = lines.next().await {
                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }
                    if line.starts_with("event: ") {
                        // Skip event type lines — we parse from data
                        continue;
                    }
                    if let Some(data) = line.strip_prefix("data: ") {
                        match decode_sse_data(data) {
                            Ok(event) => {
                                let is_done = matches!(event, ProtocolEvent::Done);
                                yield event;
                                if is_done { break; }
                            }
                            Err(_) => continue,
                        }
                    }
                }
            })
        }
    }
}
