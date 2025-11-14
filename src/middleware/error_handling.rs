use std::{fmt::Debug, future::Future};

use http_kit::{Middleware, Request, Response};
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

impl<F, Fut, Res> ErrorHandlingMiddleware<F>
where
    F: 'static + Send + Sync + Fn(crate::Error) -> Fut,
    Fut: Send + Sync + Future<Output = Res>,
    Res: Responder,
{
    /// New an error handling middleware with provided handler function.
    pub const fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F, Fut, Res> Middleware for ErrorHandlingMiddleware<F>
where
    F: 'static + Send + Sync + Fn(crate::Error) -> Fut,
    Fut: Send + Sync + Future<Output = Res>,
    Res: Responder,
{
    async fn handle(
        &mut self,
        request: &mut Request,
        mut next: impl http_kit::Endpoint,
    ) -> http_kit::Result<Response> {
        let result = next.respond(request).await;
        if let Err(error) = result {
            let mut response = Response::new(http_kit::Body::empty());
            (self.f)(error).await.respond_to(request, &mut response)?;
            return Ok(response);
        }

        result
    }
}
