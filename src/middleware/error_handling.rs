use std::{fmt::Debug, future::Future, sync::Arc};

use http_kit::{error::BoxHttpError, Body, Request, Response};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Responder,
};

/// Turn an endpoint error into a response with an asynchronous function.
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
    Fut: Send + Future<Output = Res>,
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
    Fut: Send + Future<Output = Res>,
    Res: Responder,
{
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        match next.run(request).await {
            Ok(response) => Ok(response),
            Err(error) => {
                let mut response = Response::new(Body::empty());
                // Preserve the error's status; the responder may still override it.
                *response.status_mut() = error.status();
                // We have to erase the error here, since we cannot write Fn(impl HttpError) -> ...
                (self.f)(error.into_boxed_http_error())
                    .await
                    .respond_to(request, &mut response)?;
                Ok(response)
            }
        }
    }
}
