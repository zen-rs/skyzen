//! Utility for writing middleware.
//! ```ignore
//! // An implement of timeout middleware
//! use async_std::future::timeout;
//! use std::time::Duration;
//! use async_trait::async_trait;
//! use http_kit::{Request,Response,middleware::{Middleware,Next}};
//! struct TimeOut(Duration);
//!
//! impl Middleware for TimeOut{
//!     async fn call_middleware(&self, request: &mut Request, next: Next<'_>) -> http_kit::Result<Response>{
//!         timeout(self.0,next.run(request)).await?
//!     }
//! }
//! ```
mod error_handling;
pub use error_handling::ErrorHandlingMiddleware;
pub use http_kit::middleware::Middleware;

use http_kit::{Request, Response, Result};

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
    pub fn run(self, _request: &mut Request) -> Result<Response> {
        // Placeholder implementation
        Ok(Response::new(http_kit::Body::from("Middleware not fully implemented")))
    }
}
