use crate::agent::{Agent, Layer};
use futures::{Stream, StreamExt};
use std::future::Future;

/// Logging layer — prints request/response events to stdout
pub struct LoggingLayer;

pub struct LoggingAgent<A> {
    inner: A,
}

impl<A: Agent> Layer<A> for LoggingLayer
where
    A::Request: std::fmt::Debug,
    A::Response: std::fmt::Debug,
    A::Error: std::fmt::Debug,
{
    type Output = LoggingAgent<A>;
    fn layer(self, inner: A) -> LoggingAgent<A> {
        LoggingAgent { inner }
    }
}

impl<A: Agent> Agent for LoggingAgent<A>
where
    A::Request: std::fmt::Debug,
    A::Response: std::fmt::Debug,
    A::Error: std::fmt::Debug,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    fn chat(
        &self,
        req: Self::Request,
    ) -> impl Future<Output = Result<impl Stream<Item = Self::Response>, Self::Error>> {
        eprintln!("[remi] → request: {:?}", req);
        async move {
            let stream = self.inner.chat(req).await?;
            Ok(stream.inspect(|item| eprintln!("[remi] ← response: {:?}", item)))
        }
    }
}
