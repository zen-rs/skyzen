use http_kit::{Request, Response};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, RequestBodyLimit,
};

/// Override how many request-body bytes the body extractors may buffer.
///
/// Every request the router dispatches carries a [`RequestBodyLimit`] extension, defaulting to
/// [`RequestBodyLimit::DEFAULT`] (2 MiB). Attach this middleware to raise, lower or lift that cap
/// for the routes it covers:
///
/// ```rust
/// use skyzen::{middleware::BodyLimit, routing::{CreateRouteNode, Route}, Result};
///
/// let route = Route::new((
///     "/upload".post(|| async { Result::Ok("stored") }),
/// ))
/// .with(BodyLimit::disabled());
/// ```
///
/// This middleware only publishes the limit; the extension is the contract. Body extractors
/// (`Bytes`, `ByteStr`, `Json`, `Form`, `Multipart`) are expected to read it back with
/// [`RequestBodyLimit::of`] and reject an oversized payload with `413 Payload Too Large` — that
/// enforcement is not in place yet, so today the limit is advertised but not applied.
#[derive(Debug, Clone, Copy, Default)]
pub struct BodyLimit(RequestBodyLimit);

impl BodyLimit {
    /// Cap request bodies at `max_bytes`.
    #[must_use]
    pub const fn max(max_bytes: usize) -> Self {
        Self(RequestBodyLimit::new(max_bytes))
    }

    /// Remove the cap entirely, for endpoints that accept large uploads.
    #[must_use]
    pub const fn disabled() -> Self {
        Self(RequestBodyLimit::disabled())
    }

    /// The limit this middleware publishes.
    #[must_use]
    pub const fn limit(self) -> RequestBodyLimit {
        self.0
    }
}

impl Middleware for BodyLimit {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        request.extensions_mut().insert(self.0);
        next.run(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::BodyLimit;
    use crate::{
        routing::{CreateRouteNode, Route},
        Body, Request, RequestBodyLimit, Result,
    };

    /// Reports the limit in force, so a test can observe what the router published.
    async fn report(limit: RequestBodyLimit) -> Result<String> {
        Ok(limit
            .max_bytes()
            .map_or_else(|| "disabled".to_owned(), |bytes| bytes.to_string()))
    }

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    #[tokio::test]
    async fn router_publishes_the_default_limit() {
        let router = Route::new(("/limit".at(report),)).build();
        let response = router.go(get("/limit")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, RequestBodyLimit::DEFAULT.to_string());
    }

    #[tokio::test]
    async fn route_middleware_overrides_the_default() {
        let router = Route::new(("/limit".at(report),))
            .with(BodyLimit::max(64))
            .build();
        let response = router.go(get("/limit")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "64");
    }

    #[tokio::test]
    async fn the_limit_can_be_lifted() {
        let router = Route::new(("/limit".at(report),))
            .layer(BodyLimit::disabled())
            .build();
        let response = router.go(get("/limit")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "disabled");
    }
}
