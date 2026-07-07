//! Pluggable HTTP transport layer.
//!
//! Defines the [`HttpTransport`] trait that abstracts over HTTP clients.
//! The library ships with a [`ReqwestTransport`] implementation (behind
//! the `http-client` feature) for native targets.
//!
//! On `wasm32-wasip2`, the host runtime can inject its own transport by
//! implementing this trait and passing it to [`OpenAIClient::with_transport()`].
//!
//! ```text
//! native          → ReqwestTransport (reqwest, tokio)
//! wasm32-wasip2   → host-injected impl (e.g. wasmtime-wasi-http bridge)
//! wasm32-unknown  → any wasm-compatible impl (gloo-net, fetch-api, …)
//! ```

#[cfg(feature = "http-client")]
mod reqwest_impl;

use futures::Stream;
use std::future::Future;
use std::pin::Pin;

// ── Error ─────────────────────────────────────────────────────────────────────

// Re-export HttpTransportError from remi-core (canonical definition).
pub use remi_core::error::HttpTransportError;

// ── Conditional Send ──────────────────────────────────────────────────────────

// On native targets we need Send for async tasks across threads.
// On wasm32 (single-threaded) Send is vacuously implemented but JS futures
// (JsFuture, fetch, etc.) don't impl Send, so we drop the bound there.

#[cfg(not(target_arch = "wasm32"))]
mod send_bounds {
    pub trait MaybeSend: Send {}
    impl<T: Send> MaybeSend for T {}
}

#[cfg(target_arch = "wasm32")]
mod send_bounds {
    pub trait MaybeSend {}
    impl<T> MaybeSend for T {}
}

pub use send_bounds::MaybeSend;

// ── Streaming Response ────────────────────────────────────────────────────────

/// An HTTP response with a streaming body.
///
/// The `body` field is an async stream of byte chunks, enabling SSE / streaming
/// parsing without buffering the entire response.
pub struct HttpStreamingResponse {
    /// HTTP status code (e.g. 200, 400, 500).
    pub status: u16,
    /// Response headers as `(name, value)` pairs.
    pub headers: Vec<(String, String)>,
    /// Streaming body — each item is a chunk of bytes.
    #[cfg(not(target_arch = "wasm32"))]
    pub body: Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpTransportError>> + Send>>,
    #[cfg(target_arch = "wasm32")]
    pub body: Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpTransportError>>>>,
}

// ── Transport Trait ───────────────────────────────────────────────────────────

/// Abstract HTTP transport — swap between reqwest, wasi:http, browser fetch, etc.
///
/// Implementations must be `Clone + Send + Sync + 'static` so they
/// can be stored inside [`OpenAIClient`] and shared across async tasks.
///
/// # Streaming
///
/// The response body is returned as `Pin<Box<dyn Stream<Item = Result<Vec<u8>,
/// HttpTransportError>> + Send>>` to preserve full async streaming capability.
/// This allows SSE (Server-Sent Events) to be parsed incrementally without
/// buffering the entire response.
///
/// # Example implementation
///
/// ```ignore
/// #[derive(Clone)]
/// struct MyTransport;
///
/// impl HttpTransport for MyTransport {
///     fn post_streaming(
///         &self,
///         url: String,
///         headers: Vec<(String, String)>,
///         body: Vec<u8>,
///     ) -> impl Future<Output = Result<HttpStreamingResponse, HttpTransportError>> {
///         async move {
///             // ... make HTTP POST request ...
///             Ok(HttpStreamingResponse {
///                 status: 200,
///                 body: Box::pin(futures::stream::empty()),
///             })
///         }
///     }
/// }
/// ```
#[cfg(not(target_arch = "wasm32"))]
pub trait HttpTransport: Clone + Send + Sync + 'static {
    /// Send an HTTP POST request and return a streaming response.
    fn post_streaming(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<HttpStreamingResponse, HttpTransportError>> + Send;
}

#[cfg(target_arch = "wasm32")]
pub trait HttpTransport: Clone + 'static {
    /// Send an HTTP POST request and return a streaming response.
    fn post_streaming(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<HttpStreamingResponse, HttpTransportError>>;
}

// ── SSE Line Splitter ─────────────────────────────────────────────────────────

/// Split a byte stream into SSE lines.
///
/// This is transport-agnostic — works with any `Stream<Item = Result<Vec<u8>, …>>`.
/// Replaces the previous `tokio_util::StreamReader` + `tokio::io::AsyncBufReadExt`
/// approach, making SSE parsing work across all targets.
#[cfg(not(target_arch = "wasm32"))]
pub fn sse_lines(
    body: Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpTransportError>> + Send>>,
) -> Pin<Box<dyn Stream<Item = String> + Send>> {
    use futures::StreamExt;

    Box::pin(async_stream::stream! {
        let mut body = body;
        let mut buf = Vec::new();

        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    buf.extend_from_slice(&bytes);
                    // Emit complete lines (split on '\n')
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line_bytes = buf[..pos].to_vec();
                        buf = buf[pos + 1..].to_vec();
                        // Trim trailing \r for \r\n line endings
                        let line = String::from_utf8_lossy(&line_bytes)
                            .trim_end_matches('\r')
                            .to_string();
                        yield line;
                    }
                }
                Err(_) => break,
            }
        }

        // Emit any remaining data as a final line
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).trim().to_string();
            if !line.is_empty() {
                yield line;
            }
        }
    })
}

/// Split a byte stream into SSE lines (wasm32 version — no Send bound).
#[cfg(target_arch = "wasm32")]
pub fn sse_lines(
    body: Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpTransportError>>>>,
) -> Pin<Box<dyn Stream<Item = String>>> {
    use futures::StreamExt;

    Box::pin(async_stream::stream! {
        let mut body = body;
        let mut buf = Vec::new();

        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    buf.extend_from_slice(&bytes);
                    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        let line_bytes = buf[..pos].to_vec();
                        buf = buf[pos + 1..].to_vec();
                        let line = String::from_utf8_lossy(&line_bytes)
                            .trim_end_matches('\r')
                            .to_string();
                        yield line;
                    }
                }
                Err(_) => break,
            }
        }

        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).trim().to_string();
            if !line.is_empty() {
                yield line;
            }
        }
    })
}

// ── Re-exports ────────────────────────────────────────────────────────────────

#[cfg(feature = "http-client")]
pub use reqwest_impl::ReqwestTransport;
