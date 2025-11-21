use std::{
    convert::Infallible,
    ops::{Deref, DerefMut},
};

use http::StatusCode;
use http_kit::{http_error, middleware::MiddlewareError, Middleware, Request, Response};
use skyzen_core::Extractor;

/// Share the state of application.
#[derive(Debug, Clone)]
pub struct State<T: Send + Sync + Clone + 'static>(pub T);

impl<T: Send + Sync + Clone + 'static> Deref for State<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Send + Sync + Clone + 'static> DerefMut for State<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

http_error!(
    /// An error occurred when extracting a missing state from the request extensions.
    pub StateNotExist, StatusCode::INTERNAL_SERVER_ERROR, "This state does not exist"
);

impl<T: Send + Sync + Clone + 'static> Extractor for State<T> {
    type Error = StateNotExist;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(StateNotExist::new())
    }
}

impl<T: Send + Sync + Clone + 'static> Middleware for State<T> {
    type Error = Infallible;
    async fn handle<N: http_kit::Endpoint>(
        &mut self,
        request: &mut Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        request.extensions_mut().insert(self.clone());
        next.respond(request)
            .await
            .map_err(MiddlewareError::Endpoint)
    }
}
