//! One `tracing` span per request, following OpenTelemetry field names.
//!
//! [`Trace`] wraps the whole router with [`Route::layer`](crate::routing::Route::layer), so the
//! span covers the endpoint *and* the router's own 404/405 responses. It never touches the
//! body, so streaming, SSE and WebSocket upgrades are unaffected, and latency is measured to
//! the response's headers — not to the last body byte.

use std::any::TypeId;

use http_kit::{
    header::{HeaderMap, HeaderName},
    Request, Response,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, ErrorChain, PeerAddr,
};
use tracing::{field, Instrument, Level};

use super::RequestId;
use crate::routing::MatchedPath;

// `tracing` puts the level inside a `static` callsite, so `span!`/`event!` only take a
// statically-known `Level`. Dispatching on the configured level keeps a `Level` builder knob
// possible anyway — the same workaround tower-http uses.
macro_rules! span_at {
    ($level:expr, $($arg:tt)*) => {
        match $level {
            ::tracing::Level::ERROR => ::tracing::span!(::tracing::Level::ERROR, $($arg)*),
            ::tracing::Level::WARN => ::tracing::span!(::tracing::Level::WARN, $($arg)*),
            ::tracing::Level::INFO => ::tracing::span!(::tracing::Level::INFO, $($arg)*),
            ::tracing::Level::DEBUG => ::tracing::span!(::tracing::Level::DEBUG, $($arg)*),
            ::tracing::Level::TRACE => ::tracing::span!(::tracing::Level::TRACE, $($arg)*),
        }
    };
}

macro_rules! event_at {
    ($level:expr, $($arg:tt)*) => {
        match $level {
            ::tracing::Level::ERROR => ::tracing::event!(::tracing::Level::ERROR, $($arg)*),
            ::tracing::Level::WARN => ::tracing::event!(::tracing::Level::WARN, $($arg)*),
            ::tracing::Level::INFO => ::tracing::event!(::tracing::Level::INFO, $($arg)*),
            ::tracing::Level::DEBUG => ::tracing::event!(::tracing::Level::DEBUG, $($arg)*),
            ::tracing::Level::TRACE => ::tracing::event!(::tracing::Level::TRACE, $($arg)*),
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
fn now() -> std::time::Instant {
    std::time::Instant::now()
}

#[cfg(not(target_arch = "wasm32"))]
fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// `Instant` is unavailable on `wasm32-unknown-unknown`; the platform clock is `Date`.
#[cfg(target_arch = "wasm32")]
fn now() -> f64 {
    js_sys::Date::now()
}

#[cfg(target_arch = "wasm32")]
fn elapsed_ms(started: f64) -> u64 {
    (js_sys::Date::now() - started) as u64
}

/// One `tracing` span per request, following OpenTelemetry field names.
///
/// The span carries `http.request.method`, `url.path`, `http.route`,
/// `http.response.status_code`, `request_id` and `client.address`; an event logs when the
/// response is produced and another when it fails.
///
/// ```rust
/// use skyzen::{
///     middleware::Trace,
///     routing::{CreateRouteNode, Route},
///     Result,
/// };
///
/// let router = Route::new(("/ping".at(|| async { Result::Ok("pong") }),))
///     .layer(Trace::new().with_request_id()) // one line: span + request-id layer
///     .build();
/// ```
///
/// Attach it with [`Route::layer`](crate::routing::Route::layer), not `Route::with`: a layer
/// is outermost, so the span sees unmatched 404s and 405s too. `http.route` is read from
/// [`MatchedPath`] *after* the chain returns — the request is `&mut` and shared with the
/// endpoint, so the field fills in even though the layer ran before matching.
#[derive(Debug)]
pub struct Trace {
    /// Level of the `request` span.
    level: Level,
    /// Level of the "response produced" event; `None` silences it.
    on_response: Option<Level>,
    /// Level of the event logged for 5xx responses and endpoint errors.
    on_failure: Option<Level>,
    /// Level of the event logged for 4xx responses; `None` (the default) silences it.
    on_client_error: Option<Level>,
    /// Allow-listed request headers recorded on the span as `http.request.header.<name>` —
    /// never the whole map, so `Authorization` and cookies stay out of the log.
    include_headers: Vec<HeaderName>,
    /// A bundled [`RequestId`] layer, so the common case is one `layer` call.
    request_id: Option<RequestId>,
}

impl Trace {
    /// An `INFO` span named `request`, an `INFO` event when the response is produced, and
    /// an `ERROR` event for 5xx responses and endpoint errors.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            level: Level::INFO,
            on_response: Some(Level::INFO),
            on_failure: Some(Level::ERROR),
            on_client_error: None,
            include_headers: Vec::new(),
            request_id: None,
        }
    }

    /// The level of the `request` span. Defaults to `INFO`.
    #[must_use]
    pub const fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// The level of the "response produced" event, which carries `status` and
    /// `latency_ms`. Defaults to `INFO`.
    #[must_use]
    pub const fn on_response(mut self, level: Level) -> Self {
        self.on_response = Some(level);
        self
    }

    /// The level of the failure event, emitted for 5xx responses and endpoint errors.
    /// Defaults to `ERROR`.
    #[must_use]
    pub const fn on_failure(mut self, level: Level) -> Self {
        self.on_failure = Some(level);
        self
    }

    /// Log 4xx responses at `level` too — `WARN` is the usual choice. Off by default.
    #[must_use]
    pub const fn on_client_error(mut self, level: Level) -> Self {
        self.on_client_error = Some(level);
        self
    }

    /// Record the listed request headers on the span.
    ///
    /// An allow-list, never the whole map: `Authorization` and cookies do not belong in a
    /// log line.
    #[must_use]
    pub fn include_headers(mut self, names: impl IntoIterator<Item = HeaderName>) -> Self {
        self.include_headers.extend(names);
        self
    }

    /// Bundle [`RequestId::new()`] inside this layer, so the common case — a span plus a
    /// request id — is one `layer` call instead of two.
    #[must_use]
    pub fn with_request_id(mut self) -> Self {
        self.request_id = Some(RequestId::new());
        self
    }

    /// Bundle a configured [`RequestId`] — a platform preset such as
    /// [`RequestId::cloudflare`] — inside this layer.
    #[must_use]
    pub fn with_request_id_from(mut self, request_id: RequestId) -> Self {
        self.request_id = Some(request_id);
        self
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for Trace {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let started = now();

        let span = span_at!(
            self.level,
            "request",
            http.request.method = request.method().as_str(),
            url.path = request.uri().path(),
            http.route = field::Empty,
            http.response.status_code = field::Empty,
            request_id = field::Empty,
            client.address = field::Empty,
            headers = field::Empty,
        );
        // The socket peer, not `Forwarded`/XFF: those headers are client-controlled and a
        // spoofed `client.address` in the span is worse than none.
        if let Some(peer) = request.extensions().get::<PeerAddr>() {
            span.record("client.address", field::display(peer.0.ip()));
        }
        let allow_listed = render_headers(request.headers(), &self.include_headers);
        if !allow_listed.is_empty() {
            span.record("headers", field::display(allow_listed));
        }

        // A bundled RequestId runs inside the span so its work is traced too; the id lands
        // in the extensions where the handler and the span both find it.
        let bundled = self.request_id.as_ref().map(|id| id.provide(request));

        let mut result = next.run(request).instrument(span.clone()).await;

        if let Some(path) = request.extensions().get::<MatchedPath>() {
            span.record("http.route", path.as_str());
        } else {
            span.record("http.route", "<unmatched>");
        }
        if let Some(id) = request.extensions().get::<RequestId>() {
            span.record("request_id", id.as_ref());
        }
        let latency_ms = elapsed_ms(started);

        // Events are emitted inside the span's scope so they are its children, not
        // siblings — `instrument` only made the span current while `next.run` was polled.
        span.in_scope(|| match &mut result {
            Ok(response) => {
                let status = response.status();
                span.record("http.response.status_code", i64::from(status.as_u16()));
                if let Some(level) = self.on_response {
                    event_at!(level, status = status.as_u16(), latency_ms, "response");
                }
                if status.is_server_error() {
                    if let Some(level) = self.on_failure {
                        event_at!(level, status = status.as_u16(), latency_ms, "server error");
                    }
                } else if status.is_client_error() {
                    if let Some(level) = self.on_client_error {
                        event_at!(level, status = status.as_u16(), latency_ms, "client error");
                    }
                }
                if let (Some(middleware), Some(id)) = (&self.request_id, &bundled) {
                    middleware.stamp(response, id);
                }
            }
            Err(error) => {
                span.record(
                    "http.response.status_code",
                    i64::from(error.status().as_u16()),
                );
                if let Some(level) = self.on_failure {
                    event_at!(
                        level,
                        status = error.status().as_u16(),
                        error = %ErrorChain(&*error),
                        latency_ms,
                        "request failed"
                    );
                }
            }
        });

        result
    }

    fn provisions(&self) -> Vec<TypeId> {
        self.request_id
            .iter()
            .map(|_| TypeId::of::<RequestId>())
            .collect()
    }
}

/// The allow-listed request headers rendered for the `headers` field. Values are quoted
/// with `{:?}` so delimiters inside a header (`;`, `=`, control escapes) cannot forge a
/// second `name=value` field in the rendered text.
fn render_headers(headers: &HeaderMap, names: &[HeaderName]) -> String {
    names
        .iter()
        .filter_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| format!("{name}={value:?}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{render_headers, Trace};
    use crate::{
        middleware::RequestId,
        routing::{CreateRouteNode, MatchedPath, Route},
        Body, Error, Request, Result, StatusCode,
    };
    use http_kit::{
        header::{HeaderMap, HeaderName},
        HttpError as _,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tracing::{
        field::{Field, Visit},
        span::{Attributes, Id, Record},
        Event, Subscriber,
    };
    use tracing_subscriber::{
        layer::{Context, Layer, SubscriberExt},
        Registry,
    };

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    /// A [`Layer`] that records the `request` span's finished fields and whether each event
    /// was emitted inside that span — the observability contract `Trace` exists to keep.
    #[derive(Clone, Default)]
    struct Capture {
        spans: Arc<Mutex<Vec<HashMap<String, String>>>>,
        events: Arc<Mutex<Vec<(String, bool)>>>,
    }

    struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

    impl Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }
    }

    impl<S> Layer<S> for Capture
    where
        S: Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
            if attrs.metadata().name() != "request" {
                return;
            }
            let mut fields = HashMap::new();
            attrs.record(&mut FieldVisitor(&mut fields));
            ctx.span(id).unwrap().extensions_mut().insert(fields);
        }

        fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
            let span = ctx.span(id).unwrap();
            if span.name() == "request" {
                values.record(&mut FieldVisitor(
                    span.extensions_mut()
                        .get_mut::<HashMap<String, String>>()
                        .unwrap(),
                ));
            }
        }

        fn on_close(&self, id: Id, ctx: Context<'_, S>) {
            let span = ctx.span(&id).unwrap();
            if span.name() == "request" {
                self.spans.lock().unwrap().push(
                    span.extensions()
                        .get::<HashMap<String, String>>()
                        .unwrap()
                        .clone(),
                );
            }
        }

        fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
            let in_request_span = ctx
                .event_scope(event)
                .is_some_and(|scope| scope.from_root().any(|span| span.name() == "request"));
            let mut fields = HashMap::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.events.lock().unwrap().push((
                fields.get("message").cloned().unwrap_or_default(),
                in_request_span,
            ));
        }
    }

    #[tokio::test]
    async fn the_span_carries_route_status_and_id_and_events_are_its_children() {
        let capture = Capture::default();
        let _guard = tracing::subscriber::set_default(Registry::default().with(capture.clone()));

        let router = Route::new(("/users/{id}".at(|| async { Result::Ok("u") }),))
            .layer(Trace::new().with_request_id())
            .build();
        let response = router.go(get("/users/42")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(_guard);

        let spans = capture.spans.lock().unwrap();
        let span = spans.first().expect("the request span was recorded");
        assert_eq!(span["http.route"], "/users/{id}");
        assert_eq!(span["http.response.status_code"], "200");
        assert_eq!(span["http.request.method"], "GET");
        assert_eq!(span["request_id"].len(), 36);
        drop(spans);

        let events = capture.events.lock().unwrap();
        let response_event = events
            .iter()
            .find(|(message, _)| message == "response")
            .expect("the response event fired");
        assert!(response_event.1, "events are children of the request span");
    }

    #[tokio::test]
    async fn unmatched_requests_record_route_as_unmatched() {
        let capture = Capture::default();
        let _guard = tracing::subscriber::set_default(Registry::default().with(capture.clone()));

        let router = Route::new(("/known".at(|| async { Result::Ok("ok") }),))
            .layer(Trace::new())
            .build();
        let response = router.go(get("/nope")).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        drop(_guard);

        let span = capture.spans.lock().unwrap();
        assert_eq!(span[0]["http.route"], "<unmatched>");
        assert_eq!(span[0]["http.response.status_code"], "404");
    }

    #[test]
    fn allow_listed_headers_are_quoted_so_delimiters_cannot_forge_fields() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tag", "ok; admin=true".parse().unwrap());
        let rendered = render_headers(&headers, &[HeaderName::from_static("x-tag")]);
        assert_eq!(rendered, "x-tag=\"ok; admin=true\"");
    }

    #[tokio::test]
    async fn a_wrapped_request_gets_an_id_and_matched_path() {
        let router = Route::new(("/users/{id}".at(
            |id: RequestId, route: MatchedPath| async move {
                Result::Ok(format!("{} @ {}", id.as_ref(), route.as_str()))
            },
        ),))
        .layer(Trace::new().with_request_id())
        .build();

        let response = router.go(get("/users/42")).await.unwrap();
        assert_eq!(
            response
                .headers()
                .get("x-request-id")
                .unwrap()
                .to_str()
                .unwrap()
                .len(),
            36
        );
        let body = response.into_body().into_string().await.unwrap();
        assert!(body.ends_with(" @ /users/{id}"), "{body}");
    }

    #[tokio::test]
    async fn unmatched_requests_still_pass_through_the_layer() {
        let router = Route::new(("/known".at(|| async { Result::Ok("ok") }),))
            .layer(Trace::new())
            .build();

        let response = router.go(get("/nope")).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn errors_from_the_endpoint_propagate() {
        let router =
            Route::new(("/boom".at(|| async { Result::<&'static str>::Err(Error::msg("boom")) }),))
                .layer(Trace::new())
                .build();

        let error = router.go(get("/boom")).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn bundled_request_id_satisfies_the_requirement() {
        Route::new(("/me".at(|id: RequestId| async move { Result::Ok(id.to_string()) }),))
            .layer(Trace::new().with_request_id())
            .try_build()
            .expect("Trace::with_request_id provides RequestId");
    }
}
