//! One `tracing` span per request, with OpenTelemetry field names.
//!
//! [`Trace::new`] opens a `request` span carrying `http.request.method`, `url.path` and an
//! allow-list of request headers, runs the rest of the chain inside it, then records
//! `http.route` (the matched template, not the raw path), `http.response.status_code`,
//! `request_id` and `client.address`. Latency is measured to the response *headers*, so a
//! streaming body is not part of the number.

use core::any::TypeId;
use std::future::Future;

use http_kit::{header::HeaderName, Request, Response};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor,
};
use tracing::{field::Empty, Instrument, Level, Span};

use crate::{extract::ClientIp, routing::MatchedPath};

use super::request_id::{RequestId, RequestIdLayer};

/// A started clock; [`elapsed_ms`] is what the response event reports. `Instant` is unavailable
/// on `wasm32-unknown-unknown`, so the Worker build reads `Date.now()` instead.
struct Timer {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
    #[cfg(target_arch = "wasm32")]
    start: f64,
}

impl Timer {
    fn start() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self {
                start: std::time::Instant::now(),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self {
                start: js_sys::Date::now(),
            }
        }
    }

    fn elapsed_ms(&self) -> f64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.start.elapsed().as_secs_f64() * 1000.0
        }
        #[cfg(target_arch = "wasm32")]
        {
            js_sys::Date::now() - self.start
        }
    }
}

/// The per-request span and response event.
///
/// Attach with [`Route::layer`](crate::routing::Route::layer), outermost: it sees the 404 and
/// 405 responses too, which is where `http.route = "<unmatched>"` comes from. Pair with
/// [`RequestId::new`] inside it — the span records the id after the inner layer mints it — or
/// bundle both with [`with_request_id`](Self::with_request_id).
///
/// ```rust
/// use skyzen::{middleware::{RequestId, Trace}, routing::{CreateRouteNode, Route}};
///
/// let router = Route::new(("/users/{id}".at(|| async { "ok" }),))
///     .layer(Trace::new())
///     .layer(RequestId::new())
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct Trace {
    level: Level,
    on_response: Option<Level>,
    on_failure: Level,
    on_client_error: Option<Level>,
    include_headers: Vec<HeaderName>,
    include_query: bool,
}

impl Default for Trace {
    fn default() -> Self {
        Self {
            level: Level::INFO,
            on_response: None,
            on_failure: Level::ERROR,
            on_client_error: None,
            include_headers: Vec::new(),
            include_query: false,
        }
    }
}

impl Trace {
    /// A `request` span at `INFO` and a `500`-is-an-error event. No response event for successes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The span's level.
    #[must_use]
    pub const fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Emit one event at `level` when the response is produced, with `status` and `latency_ms`.
    #[must_use]
    pub const fn on_response(mut self, level: Level) -> Self {
        self.on_response = Some(level);
        self
    }

    /// The level a `5xx` is logged at (`ERROR` by default).
    #[must_use]
    pub const fn on_failure(mut self, level: Level) -> Self {
        self.on_failure = level;
        self
    }

    /// Also emit an event at `level` for `4xx` responses (off by default).
    #[must_use]
    pub const fn on_client_error(mut self, level: Level) -> Self {
        self.on_client_error = Some(level);
        self
    }

    /// Record these request headers as span fields (`http.request.header.<name>`).
    ///
    /// Allow-list only by design: never pass all headers, or `Authorization` and cookies reach
    /// the log.
    #[must_use]
    pub fn include_headers(mut self, headers: impl IntoIterator<Item = HeaderName>) -> Self {
        self.include_headers.extend(headers);
        self
    }

    /// Also record the raw query string in the span's `url.query` field (off by default —
    /// query strings habitually carry tokens, signatures and other secrets).
    #[must_use]
    pub const fn include_query(mut self) -> Self {
        self.include_query = true;
        self
    }

    /// Bundle a [`RequestId`] layer inside this one, so the common pairing is one line:
    /// `.layer(Trace::new().with_request_id())`.
    #[must_use]
    pub fn with_request_id(self) -> RequestIdLayer {
        RequestId::new().with_trace(self)
    }

    /// Run `next` inside the request span and emit the configured events.
    pub(crate) async fn wrap(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> Result<Response, Error> {
        let client = ClientIp::extract(request)
            .await
            .ok()
            .map(|ip| ip.0.to_string());
        let span = self.span(request, client.as_deref());
        let timer = Timer::start();

        let result = next.run(request).instrument(span.clone()).await;

        let route = request.extensions().get::<MatchedPath>().map_or_else(
            || "<unmatched>".to_owned(),
            std::string::ToString::to_string,
        );
        span.record("http.route", route.as_str());
        if let Some(id) = request.extensions().get::<RequestId>() {
            span.record("request_id", id.as_str());
        }
        let latency_ms = timer.elapsed_ms();

        match result {
            Ok(response) => {
                let status = response.status();
                span.record("http.response.status_code", status.as_u16());
                if status.is_server_error() {
                    emit(
                        self.on_failure,
                        status.as_u16(),
                        latency_ms,
                        "response error",
                    );
                } else if status.is_client_error() {
                    if let Some(level) = self.on_client_error {
                        emit(level, status.as_u16(), latency_ms, "client error");
                    }
                }
                if let Some(level) = self.on_response {
                    emit(level, status.as_u16(), latency_ms, "response");
                }
                Ok(response)
            }
            Err(error) => {
                span.record("http.response.status_code", error.status().as_u16());
                match self.on_failure {
                    Level::ERROR => {
                        tracing::event!(Level::ERROR, error = %skyzen_core::ErrorChain(&error), latency_ms, "request failed");
                    }
                    Level::WARN => {
                        tracing::event!(Level::WARN, error = %skyzen_core::ErrorChain(&error), latency_ms, "request failed");
                    }
                    Level::INFO => {
                        tracing::event!(Level::INFO, error = %skyzen_core::ErrorChain(&error), latency_ms, "request failed");
                    }
                    Level::DEBUG => {
                        tracing::event!(Level::DEBUG, error = %skyzen_core::ErrorChain(&error), latency_ms, "request failed");
                    }
                    Level::TRACE => {
                        tracing::event!(Level::TRACE, error = %skyzen_core::ErrorChain(&error), latency_ms, "request failed");
                    }
                }
                Err(error)
            }
        }
    }

    /// The span for `request`, with the static fields filled and the dynamic ones reserved.
    fn span(&self, request: &Request, client: Option<&str>) -> Span {
        // Header names are only known at configuration time, so they cannot be declared as
        // individual span fields — tracing drops `record` calls for undeclared fields. They
        // land together in one declared field as `name: value` pairs.
        let headers = Headers(
            self.include_headers
                .iter()
                .filter_map(|name| {
                    request
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect(),
        );
        // The span macro needs a level known at compile time; dispatch the configured one.
        macro_rules! build {
            ($level:expr) => {
                tracing::span!(
                    $level,
                    "request",
                    http.request.method = request.method().as_str(),
                    url.path = request.uri().path(),
                    url.query = if self.include_query {
                        request.uri().query()
                    } else {
                        None
                    },
                    http.route = Empty,
                    http.response.status_code = Empty,
                    http.request.headers = tracing::field::display(&headers),
                    request_id = Empty,
                    client.address = client,
                )
            };
        }
        match self.level {
            Level::TRACE => build!(Level::TRACE),
            Level::DEBUG => build!(Level::DEBUG),
            Level::INFO => build!(Level::INFO),
            Level::WARN => build!(Level::WARN),
            Level::ERROR => build!(Level::ERROR),
        }
    }
}

/// The allow-listed request headers, as `name: value` pairs for one span field.
struct Headers(Vec<(String, String)>);

impl std::fmt::Display for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, (name, value)) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name}: {value}")?;
        }
        Ok(())
    }
}

/// Emit the response event at a runtime level; `event!` takes its level as a constant.
fn emit(level: Level, status: u16, latency_ms: f64, message: &'static str) {
    match level {
        Level::ERROR => tracing::event!(Level::ERROR, status, latency_ms, "{}", message),
        Level::WARN => tracing::event!(Level::WARN, status, latency_ms, "{}", message),
        Level::INFO => tracing::event!(Level::INFO, status, latency_ms, "{}", message),
        Level::DEBUG => tracing::event!(Level::DEBUG, status, latency_ms, "{}", message),
        Level::TRACE => tracing::event!(Level::TRACE, status, latency_ms, "{}", message),
    }
}

impl Middleware for Trace {
    fn handle(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> impl Future<Output = Result<Response, Error>> + Send {
        self.wrap(request, next)
    }

    fn provisions(&self) -> Vec<TypeId> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Trace;
    use crate::{
        routing::{CreateRouteNode, MatchedPath, Route},
        Body, Request, Result,
    };
    use skyzen_core::Extractor;

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().unwrap();
        request
    }

    #[tokio::test]
    async fn the_router_inserts_the_matched_template() {
        async fn route(path: MatchedPath) -> Result<String> {
            Ok(path.to_string())
        }
        let router = Route::new(("/users/{id}".at(route),)).build();
        let response = router.go(get("/users/42")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "/users/{id}");
    }

    #[tokio::test]
    async fn a_matched_path_joins_the_mount_prefix() {
        async fn route(path: MatchedPath) -> Result<String> {
            Ok(path.to_string())
        }
        let inner = Route::new(("/users/{id}".at(route),)).build();
        let router = Route::new(("/admin".nest(inner),)).build();
        let response = router.go(get("/admin/users/42")).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "/admin/users/{id}");
    }

    #[tokio::test]
    async fn unmatched_requests_have_no_matched_path() {
        let router = Route::new(("/users/{id}".at(|| async { "ok" }),)).build();
        let response = router.go(get("/missing")).await.unwrap();
        assert_eq!(response.status(), crate::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn bundled_trace_and_request_id_cover_the_404() {
        let router = Route::new(("/users/{id}".at(|| async { "ok" }),))
            .layer(Trace::new().with_request_id())
            .build();
        let response = router.go(get("/missing")).await.unwrap();
        assert!(response.headers().contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn trace_only_does_not_require_a_request_id() {
        let router = Route::new(("/ping".at(|| async { "pong" }),))
            .layer(Trace::new())
            .build();
        let response = router.go(get("/ping")).await.unwrap();
        assert_eq!(response.status(), crate::StatusCode::OK);
    }

    #[tokio::test]
    async fn matched_path_is_empty_when_extraction_runs_before_routing() {
        let mut request = get("/users/42");
        let path = MatchedPath::extract(&mut request).await.unwrap();
        assert!(path.is_empty());
    }
}
