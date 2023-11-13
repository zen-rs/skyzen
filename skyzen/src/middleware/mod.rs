//! Utility for writing middleware.
//! ```rust
//! // An implement of timeout middleware
//! use async_std::future::timeout;
//! use std::time::Duration;
//! use async_trait::async_trait;
//! use http_kit::{Request,Response,middleware::{Middleware,Next}};
//! struct TimeOut(Duration);
//! #[async_trait]
//! impl Middleware for TimeOut{
//!     async fn call_middleware(&self, request: &mut Request, next: Next<'_>) -> http_kit::Result<Response>{
//!         timeout(self.0,next.run(request)).await?
//!     }
//! }
//! ```
mod error_handling;
pub use error_handling::ErrorHandlingMiddleware;
pub use http_kit::middleware::{Middleware, Next};
