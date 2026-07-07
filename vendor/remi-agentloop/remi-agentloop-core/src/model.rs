use crate::agent::Agent;
use crate::error::AgentError;
use crate::types::{ChatRequest, ChatResponseChunk};

/// Marker trait for language model clients.
///
/// Any [`Agent`] whose associated types are `Request = ChatRequest`,
/// `Response = ChatResponseChunk`, and `Error = AgentError` automatically
/// implements `ChatModel` via a blanket impl — you don't need to implement
/// it manually.
///
/// # Implementing a custom model
///
/// ```ignore
/// use remi_agentloop_core::{agent::Agent, error::AgentError, types::*};
/// use futures::{Stream, stream};
///
/// struct MyModel { api_key: String }
///
/// impl Agent for MyModel {
///     type Request  = ChatRequest;
///     type Response = ChatResponseChunk;
///     type Error    = AgentError;
///
///     async fn chat(&self, req: ChatRequest)
///         -> Result<impl Stream<Item = ChatResponseChunk>, AgentError>
///     {
///         // call your API, parse SSE, yield chunks…
///         Ok(stream::empty())
///     }
/// }
/// // MyModel now automatically implements ChatModel
/// ```
pub trait ChatModel:
    Agent<Request = ChatRequest, Response = ChatResponseChunk, Error = AgentError>
{
}

impl<T> ChatModel for T where
    T: Agent<Request = ChatRequest, Response = ChatResponseChunk, Error = AgentError>
{
}
