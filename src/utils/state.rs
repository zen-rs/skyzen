//! State utilities module.
//! It provides a middleware and extractor for application state sharing.

use core::future::{ready, Future};
use std::{
    any::TypeId,
    ops::{Deref, DerefMut},
};

use http::StatusCode;
use http_kit::{Request, Response};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, Requirement,
};

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
    // Reading the state back out of the extensions is a synchronous clone, so the future is ready
    // on creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or_else(StateNotExist::new::<T>),
        )
    }

    fn requirements() -> Vec<Requirement> {
        vec![Requirement::of::<Self>("`.with(State(value))`")]
    }
}

impl<T: Send + Sync + Clone + 'static> Middleware for State<T> {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        request.extensions_mut().insert(self.clone());
        next.run(request).await
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<Self>()]
    }
}

#[cfg(test)]
mod tests {
    use skyzen_core::{Extractor, HttpError};

    use super::State;
    use crate::{
        routing::{CreateRouteNode, MethodFilter, Route, RouteBuildError},
        Body, Request, Result,
    };

    async fn read_state(State(value): State<String>) -> Result<String> {
        Ok(value)
    }

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    #[tokio::test]
    async fn middleware_injects_state_for_downstream_extractor() {
        let router = Route::new(("/state".at(read_state),))
            .with(State("skyzen".to_owned()))
            .build();

        let response = router.go(get("/state")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "skyzen");
    }

    #[tokio::test]
    async fn extractor_returns_internal_server_error_when_state_is_missing() {
        let mut request = Request::new(Body::empty());

        let error = State::<usize>::extract(&mut request).await.unwrap_err();

        assert_eq!(error.status(), crate::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.to_string().contains("usize"));
    }

    #[test]
    fn unwired_state_is_rejected_when_the_route_is_built() {
        let error = Route::new(("/state".at(read_state),))
            .try_build()
            .unwrap_err();

        let RouteBuildError::MissingProvision {
            path,
            method,
            requirement,
        } = &error
        else {
            panic!("expected a missing-provision error, got {error:?}");
        };
        assert_eq!(path, "/state");
        assert_eq!(*method, MethodFilter::Exact(crate::Method::GET));
        assert!(requirement
            .description()
            .contains("State<alloc::string::String>"));

        let rendered = error.to_string();
        assert!(rendered.contains("/state"), "{rendered}");
        assert!(rendered.contains("`.with(State(value))`"), "{rendered}");
    }

    #[test]
    fn state_attached_as_a_router_layer_satisfies_the_check() {
        Route::new(("/state".at(read_state),))
            .layer(State("skyzen".to_owned()))
            .try_build()
            .expect("a router layer provides the state");
    }
}
