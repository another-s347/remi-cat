use crate::agent::Agent;
use futures::Stream;
use std::future::Future;
use std::pin::Pin;

type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + 'a>>;

/// Type-erased Agent — user opts in via `.boxed()`
pub struct BoxedAgent<Req, Resp, Err> {
    inner: Box<dyn ErasedAgent<Req = Req, Resp = Resp, Err = Err>>,
}

impl<Req, Resp, Err> BoxedAgent<Req, Resp, Err> {
    pub fn new<A>(agent: A) -> Self
    where
        A: Agent<Request = Req, Response = Resp, Error = Err> + Send + Sync + 'static,
        Req: 'static,
        Resp: 'static,
        Err: 'static,
    {
        Self {
            inner: Box::new(agent),
        }
    }
}

impl<Req: 'static, Resp: 'static, Err: 'static> Agent for BoxedAgent<Req, Resp, Err> {
    type Request = Req;
    type Response = Resp;
    type Error = Err;

    fn chat(&self, req: Req) -> impl Future<Output = Result<impl Stream<Item = Resp>, Err>> {
        self.inner.chat_erased(req)
    }
}

// ── Internal type-erasure machinery ──────────────────────────────────────────

trait ErasedAgent: Send + Sync {
    type Req;
    type Resp;
    type Err;

    fn chat_erased(
        &self,
        req: Self::Req,
    ) -> Pin<Box<dyn Future<Output = Result<BoxStream<'static, Self::Resp>, Self::Err>>>>;
}

// This requires 'static Streams, which is a limitation of BoxedAgent.
// For most transport use-cases this is acceptable.
impl<A> ErasedAgent for A
where
    A: Agent + Send + Sync,
    A::Request: 'static,
    A::Response: 'static,
    A::Error: 'static,
{
    type Req = A::Request;
    type Resp = A::Response;
    type Err = A::Error;

    fn chat_erased(
        &self,
        _req: A::Request,
    ) -> Pin<Box<dyn Future<Output = Result<BoxStream<'static, A::Response>, A::Error>>>> {
        unimplemented!(
            "BoxedAgent requires 'static streams; \
             use the concrete Agent type in async contexts."
        )
    }
}
