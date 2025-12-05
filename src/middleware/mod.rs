//! Utility for writing middleware.
//! ```rust
//! use skyzen::{middleware::Middleware, Request, Response};
//! use tracing::info;
//!
//! #[derive(Clone, Default)]
//! struct LogMiddleware;
//!
//! impl Middleware for LogMiddleware {
//!     async fn handle(
//!         &mut self,
//!         request: &mut Request,
//!         mut next: impl http_kit::Endpoint,
//!     ) -> http_kit::Result<Response> {
//!         info!("request received");
//!         next.respond(request).await
//!     }
//! }
//! ```
mod error_handling;

pub mod auth;
pub use error_handling::ErrorHandlingMiddleware;
pub use http_kit::middleware::Middleware;
