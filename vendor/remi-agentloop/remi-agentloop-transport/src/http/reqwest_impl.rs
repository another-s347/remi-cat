//! [`HttpTransport`] implementation backed by [`reqwest`].
//!
//! This is the default transport for native targets — enabled by the
//! `http-client` feature.

use super::{HttpStreamingResponse, HttpTransport, HttpTransportError};
use futures::StreamExt;

/// HTTP transport powered by [`reqwest`].
///
/// # Example
///
/// ```ignore
/// use remi_agentloop::http::ReqwestTransport;
/// use remi_agentloop::model::openai::OpenAIClient;
///
/// let client = OpenAIClient::with_transport(
///     ReqwestTransport::new(),
///     "sk-...",
/// );
/// ```
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for ReqwestTransport {
    fn post_streaming(
        &self,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<HttpStreamingResponse, HttpTransportError>> + Send
    {
        let client = self.client.clone();

        async move {
            let mut req = client.post(&url).body(body);
            for (key, value) in headers {
                req = req.header(key, value);
            }

            let response = req.send().await.map_err(|e| {
                HttpTransportError::new(format!(
                    "reqwest send failed url={url} error={e} debug={e:?}"
                ))
            })?;

            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_string(), value.to_string()))
                })
                .collect();
            let stream_url = url.clone();

            let byte_stream = response.bytes_stream().map(move |result| {
                result.map(|bytes| bytes.to_vec()).map_err(|e| {
                    HttpTransportError::new(format!(
                        "reqwest stream read failed url={stream_url} error={e} debug={e:?}"
                    ))
                })
            });

            Ok(HttpStreamingResponse {
                status,
                headers,
                body: Box::pin(byte_stream),
            })
        }
    }
}
