use crate::agent::{Agent, Layer};
use futures::Stream;
use std::future::Future;

/// Retry layer — retries the Future (connection phase) on error
pub struct RetryLayer {
    pub max_retries: usize,
}

impl RetryLayer {
    pub fn new(max_retries: usize) -> Self {
        Self { max_retries }
    }
}

pub struct RetryAgent<A> {
    inner: A,
    max_retries: usize,
}

impl<A: Agent> Layer<A> for RetryLayer
where
    A::Request: Clone,
    A::Error: std::fmt::Debug,
{
    type Output = RetryAgent<A>;
    fn layer(self, inner: A) -> RetryAgent<A> {
        RetryAgent {
            inner,
            max_retries: self.max_retries,
        }
    }
}

impl<A: Agent> Agent for RetryAgent<A>
where
    A::Request: Clone,
    A::Error: std::fmt::Debug,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    fn chat(
        &self,
        req: Self::Request,
    ) -> impl Future<Output = Result<impl Stream<Item = Self::Response>, Self::Error>> {
        let max = self.max_retries;
        async move {
            let mut last_err = None;
            for attempt in 0..=max {
                match self.inner.chat(req.clone()).await {
                    Ok(stream) => return Ok(stream),
                    Err(e) => {
                        if attempt < max {
                            eprintln!("[remi] retry {}/{}: {:?}", attempt + 1, max, e);
                        }
                        last_err = Some(e);
                    }
                }
            }
            Err(last_err.unwrap())
        }
    }
}
