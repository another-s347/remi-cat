use crate::agent::Agent;
use futures::{Stream, StreamExt};
use std::future::Future;

// ── MapResponse ───────────────────────────────────────────────────────────────

pub struct MapResponse<A, F> {
    pub(crate) inner: A,
    pub(crate) f: F,
}

impl<A: Agent, F: Fn(A::Response) -> R + Clone, R> Agent for MapResponse<A, F> {
    type Request = A::Request;
    type Response = R;
    type Error = A::Error;

    fn chat(
        &self,
        req: Self::Request,
    ) -> impl Future<Output = Result<impl Stream<Item = R>, Self::Error>> {
        let f = self.f.clone();
        async move {
            let stream = self.inner.chat(req).await?;
            Ok(stream.map(move |item| f(item)))
        }
    }
}

// ── MapRequest ────────────────────────────────────────────────────────────────

pub struct MapRequest<A, F, NewReq> {
    pub(crate) inner: A,
    pub(crate) f: F,
    pub(crate) _phantom: std::marker::PhantomData<fn(NewReq)>,
}

impl<A: Agent, F: Fn(NewReq) -> A::Request + Clone, NewReq> Agent for MapRequest<A, F, NewReq> {
    type Request = NewReq;
    type Response = A::Response;
    type Error = A::Error;

    fn chat(
        &self,
        req: NewReq,
    ) -> impl Future<Output = Result<impl Stream<Item = A::Response>, Self::Error>> {
        let mapped = (self.f)(req);
        self.inner.chat(mapped)
    }
}

// ── MapErr ────────────────────────────────────────────────────────────────────

pub struct MapErr<A, F> {
    pub(crate) inner: A,
    pub(crate) f: F,
}

impl<A: Agent, F: Fn(A::Error) -> NewErr + Clone, NewErr> Agent for MapErr<A, F> {
    type Request = A::Request;
    type Response = A::Response;
    type Error = NewErr;

    fn chat(
        &self,
        req: Self::Request,
    ) -> impl Future<Output = Result<impl Stream<Item = A::Response>, NewErr>> {
        let f = self.f.clone();
        async move { self.inner.chat(req).await.map_err(f) }
    }
}
