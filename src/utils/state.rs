//! State utilities module.
//! It provides a middleware and extractor for application state sharing.

use std::{
    convert::Infallible,
    ops::{Deref, DerefMut},
};

use http::StatusCode;
use http_kit::{middleware::MiddlewareError, Middleware, Request, Response};
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

/// An error occurred when extracting a missing state from the request extensions.
#[derive(Debug)]
pub struct StateNotExist {
    type_name: &'static str,
}

impl StateNotExist {
    fn new<T>() -> Self {
        Self {
            type_name: std::any::type_name::<T>(),
        }
    }
}

impl std::fmt::Display for StateNotExist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "State of type `{}` does not exist", self.type_name)
    }
}

impl std::error::Error for StateNotExist {}

impl http_kit::HttpError for StateNotExist {
    fn status(&self) -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

impl<T: Send + Sync + Clone + 'static> Extractor for State<T> {
    type Error = StateNotExist;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or_else(StateNotExist::new::<T>)
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

#[cfg(test)]
mod tests {
    use http_kit::error::BoxHttpError;
    use http_kit::HttpError;
    use skyzen_core::Extractor;

    use super::State;
    use crate::{Body, Endpoint, Middleware, Request, Response};

    #[derive(Debug)]
    struct ReadStateEndpoint;

    impl Endpoint for ReadStateEndpoint {
        type Error = BoxHttpError;

        async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
            let State(value) = State::<String>::extract(request)
                .await
                .map_err(|error| Box::new(error) as BoxHttpError)?;
            Ok(Response::new(Body::from(value)))
        }
    }

    #[tokio::test]
    async fn middleware_injects_state_for_downstream_endpoint_and_extractor() {
        let mut middleware = State("skyzen".to_owned());
        let mut request = Request::new(Body::empty());

        let response = middleware
            .handle(&mut request, ReadStateEndpoint)
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "skyzen");

        let extracted = State::<String>::extract(&mut request).await.unwrap();
        assert_eq!(&*extracted, "skyzen");
    }

    #[tokio::test]
    async fn extractor_returns_internal_server_error_when_state_is_missing() {
        let mut request = Request::new(Body::empty());

        let error = State::<usize>::extract(&mut request).await.unwrap_err();

        assert_eq!(error.status(), crate::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.to_string().contains("usize"));
    }
}
