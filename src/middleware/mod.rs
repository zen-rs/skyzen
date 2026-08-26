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
