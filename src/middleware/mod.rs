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
pub use error_handling::ErrorHandlingMiddleware;
pub use http_kit::middleware::Middleware;

use http_kit::{error::BoxHttpError, Request, Response};

/// Simplified middleware system - just for compilation.
/// This is a placeholder implementation that needs proper redesign.
#[derive(Debug, Default, Clone, Copy)]
pub struct Next;

impl Next {
    /// Create a placeholder [`Next`] instance.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Run the faux middleware chain.
    ///
    /// # Errors
    ///
    /// This placeholder never returns an error.
    pub fn run(self, _request: &mut Request) -> Result<Response, BoxHttpError> {
        // Placeholder implementation
        Ok(Response::new(http_kit::Body::from(
            "Middleware not fully implemented",
        )))
    }
}
