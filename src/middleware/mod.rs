//! Middleware: wrap request handling to add behaviour around your endpoints.
//!
//! A middleware is a value that sees every request on its way in and every response on its way
//! out. Implement [`Middleware`] on a type when the behaviour deserves a name:
//!
//! ```rust
//! use std::sync::atomic::{AtomicUsize, Ordering};
//! use skyzen::{middleware::{Middleware, Next}, Error, Request, Response};
//!
//! #[derive(Debug, Default)]
//! struct CountRequests {
//!     seen: AtomicUsize,
//! }
//!
//! impl Middleware for CountRequests {
//!     async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
//!         self.seen.fetch_add(1, Ordering::Relaxed);
//!         next.run(request).await
//!     }
//! }
//! ```
//!
//! The middleware value is shared, not cloned per request, so the counter above really counts.
//! For one-off behaviour, [`from_fn`] takes a closure instead:
//!
//! ```rust
//! use skyzen::middleware::from_fn;
//!
//! let log = from_fn(|request, next| {
//!     Box::pin(async move {
//!         tracing::info!(path = request.uri().path(), "request received");
//!         next.run(request).await
//!     })
//! });
//! ```
//!
//! Attach middleware with [`Route::with`](crate::routing::Route::with) for a whole subtree,
//! [`RouteNode::with`](crate::routing::RouteNode::with) for a single node, or
//! [`Route::layer`](crate::routing::Route::layer) to wrap the entire router — including its
//! 404 and 405 responses, which is what CORS and tracing layers need.

mod body_limit;
mod cors;
mod error_handling;
#[cfg(not(target_arch = "wasm32"))]
mod timeout;

pub mod auth;
pub mod compression;

use std::fmt::{self, Debug};

use http_kit::{Endpoint, Request, Response};
use skyzen_core::{
    middleware::{boxed, BoxFuture, Dispatch},
    Error,
};

pub use body_limit::BodyLimit;
pub use compression::{CompressionEncoding, CompressionLevel, CompressionMiddleware};
pub use cors::{AllowOrigin, Cors, CorsConfigError};
pub use error_handling::ErrorHandlingMiddleware;
#[doc(inline)]
pub use skyzen_core::middleware::{
    apply, from_fn, BoxMiddleware, FromFn, Middleware, MiddlewareFn, Next,
};
#[cfg(not(target_arch = "wasm32"))]
pub use timeout::Timeout;

/// An [`Endpoint`] wrapped in one [`Middleware`].
///
/// Produced by [`layer`]. Composing repeatedly nests the wrappers, so the middleware applied last
/// runs first.
pub struct Layered<E> {
    endpoint: E,
    middleware: [BoxMiddleware; 1],
}

impl<E: Debug> Debug for Layered<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Layered")
            .field("endpoint", &self.endpoint)
            .field("middleware", &self.middleware[0].middleware_name())
            .finish()
    }
}

impl<E: Clone> Clone for Layered<E> {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            middleware: self.middleware.clone(),
        }
    }
}

impl<E> Layered<E> {
    /// The endpoint this layer wraps.
    ///
    /// Layering is how `#[skyzen::main]` injects the manifest's services, so the router an
    /// application built is usually several `Layered`s deep by the time the runtime sees it. This
    /// is what lets a question about the endpoint — which routes does it serve? — still reach it.
    pub const fn endpoint(&self) -> &E {
        &self.endpoint
    }
}

/// Wrap `endpoint` so `middleware` runs around every request it serves.
///
/// Prefer [`Route::layer`](crate::routing::Route::layer) when the endpoint is a router: layers
/// registered there also cover the router's fallback responses and take part in build-time wiring
/// validation.
pub fn layer<E, M>(endpoint: E, middleware: M) -> Layered<E>
where
    E: Endpoint + Clone + Send + Sync + 'static,
    M: Middleware,
{
    Layered {
        endpoint,
        middleware: [boxed(middleware)],
    }
}

impl<E> Endpoint for Layered<E>
where
    E: Endpoint + Clone + Send + Sync + 'static,
{
    type Error = http_kit::error::BoxHttpError;

    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let terminal = CloneEndpoint(&self.endpoint);
        Next::new(&self.middleware, &terminal)
            .run(request)
            .await
            .map_err(Error::into_boxed_http_error)
    }
}

/// Chain terminal that serves a request from a fresh clone of a shared endpoint.
struct CloneEndpoint<'e, E>(&'e E);

impl<E> Dispatch for CloneEndpoint<'_, E>
where
    E: Endpoint + Clone + Send + Sync + 'static,
{
    fn dispatch<'a>(&'a self, request: &'a mut Request) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let mut endpoint = self.0.clone();
            endpoint.respond(request).await.map_err(Error::from)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{layer, Middleware, Next};
    use crate::{
        routing::{CreateRouteNode, Route},
        Body, Endpoint, Error, Request, Response, Result,
    };
    use http_kit::header::{HeaderName, HeaderValue};

    /// Stands in for the service injectors `#[skyzen::main]` wraps a built router with.
    #[derive(Debug)]
    struct Stamp(&'static str);

    impl Middleware for Stamp {
        async fn handle(
            &self,
            request: &mut Request,
            next: Next<'_>,
        ) -> std::result::Result<Response, Error> {
            let mut response = next.run(request).await?;
            response.headers_mut().append(
                HeaderName::from_static("x-stamp"),
                HeaderValue::from_static(self.0),
            );
            Ok(response)
        }
    }

    #[tokio::test]
    async fn layering_an_endpoint_applies_the_last_wrapper_outermost() {
        let router = Route::new(("/ping".at(|| async { Result::Ok("pong") }),)).build();
        let mut endpoint = layer(layer(router, Stamp("inner")), Stamp("outer"));

        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/ping".parse().expect("valid path");
        let response = endpoint.respond(&mut request).await.unwrap();

        // Both wrappers ran; the outer one appended last because it saw the response last.
        let stamps: Vec<&str> = response
            .headers()
            .get_all("x-stamp")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(stamps, ["inner", "outer"]);
    }
}
