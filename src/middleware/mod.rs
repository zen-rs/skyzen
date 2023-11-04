//! Utility for writing middleware.
//!
//! There're mutiple way to write a middleware for skyzen.
//!
//! # Tranform a handler to middleware
//! ```rust
//! use skyzen::middleware::into_middleware;
//! async fn handler(){
//!     
//! }
//! 
//! into_middleware(handler)
//! ```
//!
//! This approach is very simple, but not flexible. So in some case, we should implement this trait directly.
//!
//! # Implement `Middleware` trait directly
//!
//! ```rust
//! // An implement of timeout middleware
//! use async_std::future::timeout;
//! use std::time::Duration;
//! use async_trait::async_trait;
//! use http_kit::{Request,middleware::{Middleware,Next}};
//! struct TimeOut(Duration);

//! #[async_trait]
//! impl Middleware for TimeOut{
//!     async fn call_middleware(&self, request: &mut Request, next: Next<'_>) -> Result<Response>{
//!         timeout(self.duration,next.run(request)).await?
//!     }
//! }
//! ```
mod error_handling;
use std::marker::PhantomData;

use async_trait::async_trait;
pub use error_handling::ErrorHandlingMiddleware;
pub use http_kit::middleware::Middleware;
pub use http_kit::middleware::Next;
use http_kit::Request;
use http_kit::Response;
use skyzen_core::Extractor;

use crate::handler::Handler;

struct IntoMiddleware<H: Handler<T>, T: Extractor> {
    handler: H,
    _marker: PhantomData<T>,
}

/// Transform handler to middleware.
pub fn into_middleware<T: Extractor + Send + Sync>(handler: impl Handler<T>) -> impl Middleware {
    IntoMiddleware::new(handler)
}

impl<H: Handler<T>, T: Extractor + Send + Sync> IntoMiddleware<H, T> {
    /// Create an `IntoMiddleware` instance.
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            _marker: PhantomData,
        }
    }
}

#[async_trait]
impl<H: Handler<T>, T: Extractor + Send + Sync> Middleware for IntoMiddleware<H, T> {
    async fn call_middleware(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> http_kit::Result<Response> {
        self.handler.call_handler(request).await?;
        next.run(request).await
    }
}
