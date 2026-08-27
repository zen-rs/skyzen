use std::time::Duration;

use futures_util::future::{select, Either};
use http_kit::{http_error, Request, Response, StatusCode};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error,
};

http_error!(
    /// Raised when an endpoint did not answer within its [`Timeout`].
    pub RequestTimeout,
    StatusCode::REQUEST_TIMEOUT,
    "The request took too long to process."
);

/// Abandon a request that takes longer than the configured budget.
///
/// The expired request is answered with `408 Request Timeout` rather than `503 Service
/// Unavailable`: the deadline is a property of this request, and a caller retrying a smaller or
/// simpler request is the useful next step — which is exactly what `408` tells them.
///
/// ```rust
/// use std::time::Duration;
/// use skyzen::{middleware::Timeout, routing::{CreateRouteNode, Route}, Result};
///
/// let router = Route::new((
///     "/report".at(|| async { Result::Ok("done") }),
/// ))
/// .layer(Timeout::new(Duration::from_secs(5)))
/// .build();
/// ```
///
/// Native targets only: WebAssembly isolates cap request duration themselves, and the platform
/// timer skyzen would need is not available there.
#[derive(Debug, Clone, Copy)]
pub struct Timeout(Duration);

impl Timeout {
    /// Give each request `budget` to produce a response.
    #[must_use]
    pub const fn new(budget: Duration) -> Self {
        Self(budget)
    }

    /// The configured budget.
    #[must_use]
    pub const fn budget(self) -> Duration {
        self.0
    }
}

impl Middleware for Timeout {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let inner = std::pin::pin!(next.run(request));
        let deadline = std::pin::pin!(async_io::Timer::after(self.0));

        match select(inner, deadline).await {
            Either::Left((response, _)) => response,
            Either::Right((_, _)) => Err(Error::from(RequestTimeout::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Timeout;
    use crate::{
        routing::{CreateRouteNode, Route},
        Body, Request, Result, StatusCode,
    };
    use std::time::Duration;

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    #[tokio::test]
    async fn a_slow_endpoint_is_abandoned_with_408() {
        async fn slow() -> Result<&'static str> {
            async_io::Timer::after(Duration::from_secs(30)).await;
            Ok("never")
        }

        let router = Route::new(("/slow".at(slow),))
            .layer(Timeout::new(Duration::from_millis(10)))
            .build();

        let error = router.go(get("/slow")).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn a_prompt_endpoint_is_untouched() {
        let router = Route::new(("/fast".at(|| async { Result::Ok("done") }),))
            .layer(Timeout::new(Duration::from_secs(30)))
            .build();

        let response = router.go(get("/fast")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
