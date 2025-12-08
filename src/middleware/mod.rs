//! Utility for writing middleware.
//! ```rust
//! use skyzen::{middleware::Middleware, Request, Response};
//! use tracing::info;
//!
//! #[derive(Clone, Default)]
//! struct LogMiddleware;
//!
//! impl Middleware for LogMiddleware {
//!     type Error = http_kit::Error;
//!     async fn handle<E: http_kit::Endpoint>(
//!         &mut self,
//!         request: &mut Request,
//!         mut next: E,
//!     ) -> http_kit::Result<Response, http_kit::middleware::MiddlewareError<E::Error, Self::Error>>
//!     {
//!         info!("request received");
//!         next.respond(request)
//!             .await
//!             .map_err(http_kit::middleware::MiddlewareError::Endpoint)
//!     }
//! }
//! ```
mod error_handling;

pub mod auth;
pub use error_handling::ErrorHandlingMiddleware;
pub use http_kit::middleware::Middleware;
