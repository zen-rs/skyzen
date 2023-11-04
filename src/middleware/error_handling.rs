use std::{fmt::Debug, future::Future};

use async_trait::async_trait;
use http_kit::{middleware::Next, Middleware, Request, Response};
use skyzen_core::Responder;

/// Handler error with an asynchronous function
pub struct ErrorHandlingMiddleware<F> {
    f: F,
}

impl<F> Debug for ErrorHandlingMiddleware<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ErrorHandlingMiddleware")
    }
}

impl<F: Send + Sync, Fut: Send, Res> ErrorHandlingMiddleware<F>
where
    F: 'static + Fn(crate::Error) -> Fut,
    Fut: Future<Output = Res>,
    Res: Responder,
{
    /// New an error handling middleware with provided handler function.
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait]
impl<F: Send + Sync, Fut: Send, Res> Middleware for ErrorHandlingMiddleware<F>
where
    F: 'static + Fn(crate::Error) -> Fut,
    Fut: Future<Output = Res>,
    Res: Responder,
{
    async fn call_middleware(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> crate::Result<Response> {
        let result = next.run(request).await;
        if let Err(error) = result {
            let mut response = Response::empty();
            (self.f)(error).await.respond_to(request, &mut response)?;
            return Ok(response);
        }

        result
    }
}
