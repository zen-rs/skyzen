use std::{fmt::Debug, future::Future, sync::Arc};

use http_kit::{Middleware, Request, Response, error::BoxHttpError, middleware::MiddlewareError};
use skyzen_core::Responder;

/// Handler error with an asynchronous function
pub struct ErrorHandlingMiddleware<F> {
    f: Arc<F>,
}

impl<F> Clone for ErrorHandlingMiddleware<F> {
    fn clone(&self) -> Self {
        Self {
            f: Arc::clone(&self.f),
        }
    }
}

impl<F> Debug for ErrorHandlingMiddleware<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ErrorHandlingMiddleware")
    }
}

impl<F, Fut, Res> ErrorHandlingMiddleware<F>
where
    F: 'static + Send + Sync + Fn(BoxHttpError) -> Fut,
    Fut: Send + Sync + Future<Output = Res>,
    Res: Responder,
{
    /// New an error handling middleware with provided handler function.
    pub fn new(f: F) -> Self {
        Self { f: Arc::new(f) }
    }
}

impl<F, Fut, Res> Middleware for ErrorHandlingMiddleware<F>
where
    F: 'static + Send + Sync + Fn(BoxHttpError) -> Fut,
    Fut: Send + Sync + Future<Output = Res>,
    Res: Responder,
{
    type Error = Res::Error;
    async fn handle<N: http_kit::Endpoint>(
        &mut self,
        request: &mut Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        match next.respond(request).await {
            Ok(response) => Ok(response),
            Err(error) => {
                let mut response = Response::new(http_kit::Body::empty());
                // We have to erase the error here, since we cannot write Fn(impl HttpError) -> ...
                (self.f)(Box::new(error) as BoxHttpError)
                    .await
                    .respond_to(request, &mut response)
                    .map_err(MiddlewareError::Middleware)?;
                Ok(response)
            }
        }
    }
}
