use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use axum::Json;
use futures::{Stream, StreamExt};

use remi_core::protocol::{ProtocolError, ProtocolEvent};
use remi_core::types::LoopInput;

/// HTTP SSE server for exposing an agent over HTTP.
///
/// Uses a closure-based API so the compiler can verify `Send` bounds
/// on the concrete stream type (which RPITIT trait bounds cannot express
/// across crate boundaries).
///
/// # Example
///
/// ```ignore
/// let agent = Arc::new(my_agent);
/// let server = HttpSseServer::new(move |req| {
///     let agent = agent.clone();
///     async move { agent.chat(req).await }
/// });
/// server.bind(([0, 0, 0, 0], 8080)).serve().await?;
/// ```
pub struct HttpSseServer<F> {
    handler: Arc<F>,
    bind_addr: SocketAddr,
}

impl<F, Fut, S> HttpSseServer<F>
where
    F: Fn(LoopInput) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<S, ProtocolError>> + Send + 'static,
    S: Stream<Item = ProtocolEvent> + Send + 'static,
{
    pub fn new(handler: F) -> Self {
        Self {
            handler: Arc::new(handler),
            bind_addr: ([0, 0, 0, 0], 8080).into(),
        }
    }

    pub fn bind(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.bind_addr = addr.into();
        self
    }

    /// Build the axum `Router` for this server without binding.
    ///
    /// Useful when you want to merge additional routes (e.g. a static UI or
    /// a `/status` SSE endpoint) before binding and serving:
    ///
    /// ```ignore
    /// let app = server.router()
    ///     .merge(your_extra_router)
    ///     .layer(CorsLayer::permissive());
    /// let listener = tokio::net::TcpListener::bind(addr).await?;
    /// axum::serve(listener, app).await?;
    /// ```
    pub fn router(self) -> axum::Router {
        let handler = self.handler;
        axum::Router::new()
            .route("/chat", post(handle_chat::<F, Fut, S>))
            .with_state(handler)
    }

    /// Start the HTTP server.
    pub async fn serve(self) -> Result<(), std::io::Error> {
        let addr = self.bind_addr;
        let app = self.router();
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await
    }
}

async fn handle_chat<F, Fut, S>(
    State(handler): State<Arc<F>>,
    Json(req): Json<LoopInput>,
) -> Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>
where
    F: Fn(LoopInput) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<S, ProtocolError>> + Send + 'static,
    S: Stream<Item = ProtocolEvent> + Send + 'static,
{
    match handler(req).await {
        Err(e) => {
            let err_event = ProtocolEvent::Error {
                message: e.message.clone(),
                code: Some(e.code.clone()),
            };
            Sse::new(Box::pin(futures::stream::once(async move {
                Ok::<Event, Infallible>(protocol_event_to_sse(&err_event))
            })))
        }
        Ok(stream) => {
            Sse::new(Box::pin(stream.map(|event| {
                Ok::<Event, Infallible>(protocol_event_to_sse(&event))
            })))
        }
    }
}

/// Convert a ProtocolEvent into an axum SSE Event with proper event type + JSON data.
fn protocol_event_to_sse(event: &ProtocolEvent) -> Event {
    let event_type = match event {
        ProtocolEvent::RunStart { .. } => "run_start",
        ProtocolEvent::Delta { .. } => "delta",
        ProtocolEvent::ThinkingStart => "thinking_start",
        ProtocolEvent::ThinkingEnd { .. } => "thinking_end",
        ProtocolEvent::ToolCallStart { .. } => "tool_call_start",
        ProtocolEvent::ToolCallDelta { .. } => "tool_call_delta",
        ProtocolEvent::ToolDelta { .. } => "tool_delta",
        ProtocolEvent::ToolResult { .. } => "tool_result",
        ProtocolEvent::SubSession { .. } => "sub_session",
        ProtocolEvent::Interrupt { .. } => "interrupt",
        ProtocolEvent::TurnStart { .. } => "turn_start",
        ProtocolEvent::Usage { .. } => "usage",
        ProtocolEvent::Error { .. } => "error",
        ProtocolEvent::Done => "done",
        ProtocolEvent::Cancelled => "cancelled",
        ProtocolEvent::NeedToolExecution { .. } => "need_tool_execution",
        ProtocolEvent::Custom { event_type, .. } => event_type.as_str(),
    };
    let data = serde_json::to_string(event).unwrap_or_default();
    Event::default().event(event_type).data(data)
}
