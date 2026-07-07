use futures::Stream;

/// Core async-streaming agent trait — fully generic, no `Send` bound.
///
/// An `Agent` is anything that accepts a `Request` and streams back a
/// sequence of `Response` items.  The trait is deliberately minimal; richer
/// behaviour (memory, tool execution, checkpointing) is layered on top via
/// [`AgentExt`], [`Layer`], and [`AgentBuilder`](crate::builder::AgentBuilder).
///
/// The absence of `Send` bounds lets the same trait work on `wasm32` targets
/// where JS-backed futures are `!Send`.
///
/// # The concrete pipeline
///
/// The framework ships two concrete `Agent` implementations:
///
/// - [`AgentLoop`](crate::agent_loop::AgentLoop) —
///   `Agent<Request = LoopInput, Response = AgentEvent>`.
///   Pure step + tool execution core, no memory.
/// - [`BuiltAgent`](crate::builder::BuiltAgent) —
///   wraps `AgentLoop` and adds conversation memory + run lifecycle.
///
/// # Implementing `Agent`
///
/// ```ignore
/// use remi_agentloop_core::agent::Agent;
/// use futures::{Stream, stream};
///
/// struct EchoAgent;
///
/// impl Agent for EchoAgent {
///     type Request  = String;
///     type Response = String;
///     type Error    = std::convert::Infallible;
///
///     async fn chat(
///         &self,
///         req: String,
///     ) -> Result<impl Stream<Item = String>, Self::Error> {
///         // Immediately yield the request as a single response chunk.
///         Ok(stream::once(async move { req }))
///     }
/// }
/// ```
pub trait Agent {
    /// Input type for a single invocation.
    type Request;
    /// Individual chunk / event streamed back to the caller.
    type Response;
    /// Error type returned when the conversation cannot start.
    type Error;

    /// Start a conversation and return a streaming response.
    ///
    /// The returned `Stream` is lazy — items are not produced until the caller
    /// polls the stream.  Implementations may yield any number of items
    /// (including zero) before the stream terminates.
    async fn chat(
        &self,
        req: Self::Request,
    ) -> Result<impl Stream<Item = Self::Response>, Self::Error>;
}

/// Extension methods auto-provided to every [`Agent`] via a blanket impl.
///
/// These combinators mirror the iterator/stream adapter pattern: each method
/// wraps `self` in a thin adapter struct without allocating or boxing.
///
/// # Example — response mapping
///
/// ```ignore
/// use remi_agentloop_core::prelude::*;
///
/// // Wrap every AgentEvent::TextDelta to count characters
/// let counted = agent.map_response(|event| {
///     if let AgentEvent::TextDelta(ref s) = event {
///         println!("chunk: {} chars", s.len());
///     }
///     event
/// });
/// ```
pub trait AgentExt: Agent + Sized {
    /// Map each `Response` item emitted by the inner stream.
    ///
    /// ```ignore
    /// // Prepend a prefix to every text delta
    /// let agent = agent.map_response(|event| match event {
    ///     AgentEvent::TextDelta(s) => AgentEvent::TextDelta(format!("[bot] {s}")),
    ///     other => other,
    /// });
    /// ```
    fn map_response<F, R>(self, f: F) -> crate::adapters::map::MapResponse<Self, F>
    where
        F: Fn(Self::Response) -> R,
    {
        crate::adapters::map::MapResponse { inner: self, f }
    }

    /// Transform the `Request` type before forwarding to the inner agent.
    ///
    /// ```ignore
    /// // Accept a plain &str and convert to ChatInput
    /// let agent = agent.map_request(|s: &str| ChatInput::text(s));
    /// ```
    fn map_request<F, NewReq>(self, f: F) -> crate::adapters::map::MapRequest<Self, F, NewReq>
    where
        F: Fn(NewReq) -> Self::Request,
    {
        crate::adapters::map::MapRequest {
            inner: self,
            f,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Transform the `Error` type.
    ///
    /// ```ignore
    /// let agent = agent.map_err(|e| MyError::Agent(e.to_string()));
    /// ```
    fn map_err<F, NewErr>(self, f: F) -> crate::adapters::map::MapErr<Self, F>
    where
        F: Fn(Self::Error) -> NewErr,
    {
        crate::adapters::map::MapErr { inner: self, f }
    }

    /// Wrap `self` with a [`Layer`] adapter.
    ///
    /// ```ignore
    /// use remi_agentloop_core::adapters::logging::LoggingLayer;
    ///
    /// let agent = AgentBuilder::new().model(oai).build();
    /// let logged = agent.layer(LoggingLayer::new());
    /// ```
    fn layer<L: Layer<Self>>(self, layer: L) -> L::Output {
        layer.layer(self)
    }

    /// Type-erase into a [`BoxedAgent`](crate::adapters::boxed::BoxedAgent).
    ///
    /// Use this when you need to store agents with different concrete types
    /// behind a common interface.  Requires `Self: Send + Sync + 'static`.
    ///
    /// ```ignore
    /// let agents: Vec<BoxedAgent<LoopInput, AgentEvent, AgentError>> = vec![
    ///     agent_a.boxed(),
    ///     agent_b.boxed(),
    /// ];
    /// ```
    fn boxed(self) -> crate::adapters::boxed::BoxedAgent<Self::Request, Self::Response, Self::Error>
    where
        Self: Send + Sync + 'static,
        Self::Request: 'static,
        Self::Response: 'static,
        Self::Error: 'static,
    {
        crate::adapters::boxed::BoxedAgent::new(self)
    }
}

impl<A: Agent> AgentExt for A {}

/// Middleware pattern: wraps one [`Agent`] and produces another.
///
/// Implement this trait to add cross-cutting concerns (logging, retry,
/// rate-limiting, tracing, etc.) without modifying the inner agent.
///
/// # Example — a simple logging layer
///
/// ```ignore
/// use remi_agentloop_core::agent::{Agent, Layer};
/// use remi_agentloop_core::types::AgentEvent;
/// use futures::Stream;
///
/// struct LoggingLayer;
///
/// struct LoggingAgent<A>(A);
///
/// impl<A: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError>>
///     Layer<A> for LoggingLayer
/// {
///     type Output = LoggingAgent<A>;
///     fn layer(self, inner: A) -> LoggingAgent<A> { LoggingAgent(inner) }
/// }
///
/// impl<A: Agent<Request = LoopInput, Response = AgentEvent, Error = AgentError>>
///     Agent for LoggingAgent<A>
/// {
///     type Request = LoopInput;
///     type Response = AgentEvent;
///     type Error = AgentError;
///
///     async fn chat(&self, req: LoopInput)
///         -> Result<impl Stream<Item = AgentEvent>, AgentError>
///     {
///         println!("→ starting chat");
///         self.0.chat(req).await
///     }
/// }
/// ```
pub trait Layer<A: Agent> {
    /// The agent type produced by this layer.
    type Output: Agent;
    /// Consume the layer and wrap `inner`, returning the composed agent.
    fn layer(self, inner: A) -> Self::Output;
}
