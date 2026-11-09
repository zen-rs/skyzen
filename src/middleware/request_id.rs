//! A per-request identifier: minted, optionally trusted from the inbound request, echoed back.
//!
//! [`RequestId`] is both the [`Middleware`] that assigns the id and the [`Extractor`] handlers
//! read it through, in the same way [`State`](crate::utils::State) is both the middleware and
//! the value it provides.

use std::{
    any::TypeId,
    fmt,
    future::{ready, Future},
    str::FromStr as _,
    sync::Arc,
};

use http_kit::{
    header::{HeaderName, HeaderValue},
    Request, Response, StatusCode,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, PlatformRequestId, Requirement,
};

/// The header a request id is read from and echoed back on, unless configured otherwise.
const DEFAULT_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// The default bound on a reusable inbound id.
const DEFAULT_MAX_LEN: usize = 128;

/// A source a [`RequestId`] may reuse an existing identifier from instead of minting one.
#[derive(Debug, Clone)]
enum Inbound {
    /// The `trace-id` segment of a W3C `traceparent` header.
    TraceParent,
    /// The identifier a serverless adapter put into [`PlatformRequestId`].
    Platform,
}

/// A request's unique identifier — and the middleware that assigns one.
///
/// Attached to a router with [`Route::layer`](crate::routing::Route::layer) or
/// [`Route::with`](crate::routing::Route::with), `RequestId` puts an id in the request
/// extensions for every request it sees — so handlers extract it like any other value — and
/// echoes the id on the response's request-id header, including the router's own 404 and
/// 405 responses when attached as a layer.
///
/// ```rust
/// use skyzen::{
///     middleware::RequestId,
///     routing::{CreateRouteNode, Route},
///     Result,
/// };
///
/// async fn get_user(id: RequestId) -> Result<String> {
///     tracing::info!(%id, "loading user");
///     Result::Ok("user".to_owned())
/// }
///
/// let router = Route::new(("/users/{id}".at(get_user),))
///     .layer(RequestId::new())
///     .build();
/// ```
///
/// Extracting `RequestId` declares a [`Requirement`]: route middleware is the only way the
/// id arrives, so forgetting the layer is rejected by [`Route::build`](crate::routing::Route::build)
/// rather than failing requests with a 500 at runtime.
#[derive(Debug, Clone)]
pub struct RequestId {
    id: Arc<str>,
    /// Header read inbound (when trusted) and written onto the response.
    header: HeaderName,
    trust_incoming: bool,
    /// Bound applied to an inbound id's length before it is reused.
    max_len: usize,
    /// Additional trusted inbound sources, tried in order after `header`.
    inbound: Vec<Inbound>,
}

impl RequestId {
    /// Mint a fresh `UUIDv7` per request, echoed on the `x-request-id` header.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: Arc::from(""),
            header: DEFAULT_HEADER,
            trust_incoming: false,
            max_len: DEFAULT_MAX_LEN,
            inbound: Vec::new(),
        }
    }

    /// The header the id is read from and echoed on. Defaults to `x-request-id`.
    ///
    /// # Panics
    ///
    /// Panics when `name` is not a valid HTTP header name.
    #[must_use]
    pub fn header(mut self, name: impl AsRef<str>) -> Self {
        self.header = HeaderName::from_str(name.as_ref()).expect("a valid header name");
        self
    }

    /// Reuse a valid inbound id instead of minting a new one.
    ///
    /// Off by default: a client-chosen id is a log-injection vector unless a trusted proxy
    /// sets the header. Turn it on behind one, or use a platform preset like
    /// [`RequestId::cloudflare`]. Inbound ids longer than [`max_len`](Self::max_len) or
    /// containing non-visible ASCII are replaced with a fresh id rather than trusted.
    #[must_use]
    pub const fn trust_incoming(mut self, trust: bool) -> Self {
        self.trust_incoming = trust;
        self
    }

    /// The maximum length in bytes of a reusable inbound id. Defaults to 128.
    #[must_use]
    pub const fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self
    }

    /// Reuse Cloudflare's `cf-ray` header as the request id.
    ///
    /// The ray id is minted by the edge, not the client, so it is safe to trust.
    #[must_use]
    pub fn cloudflare() -> Self {
        Self::new().header("cf-ray").trust_incoming(true)
    }

    /// Reuse `x-ms-request-id`, falling back to the `trace-id` of a W3C `traceparent`
    /// header when Azure's header is absent.
    #[must_use]
    pub fn azure() -> Self {
        let mut request_id = Self::new().header("x-ms-request-id").trust_incoming(true);
        request_id.inbound.push(Inbound::TraceParent);
        request_id
    }

    /// Reuse the Lambda request ID from the invocation context.
    ///
    /// `skyzen-lambda` copies it into [`PlatformRequestId`] before the endpoint runs. Off
    /// Lambda — in tests, on a native server — this mints a fresh `UUIDv7` instead.
    #[must_use]
    pub fn lambda() -> Self {
        let mut request_id = Self::new();
        request_id.inbound.push(Inbound::Platform);
        request_id
    }

    /// Pick the id this request carries — the first trusted inbound value, else a fresh
    /// `UUIDv7` — and store it in the request extensions for downstream extraction.
    pub(crate) fn provide(&self, request: &mut Request) -> Arc<str> {
        let id = self.resolve(request);
        request.extensions_mut().insert(Self {
            id: Arc::clone(&id),
            ..self.clone()
        });
        id
    }

    /// Write `id` onto `response`'s request-id header.
    pub(crate) fn stamp(&self, response: &mut Response, id: &str) {
        response.headers_mut().insert(
            self.header.clone(),
            HeaderValue::from_str(id).expect("a request id is always a valid header value"),
        );
    }

    /// The first trusted inbound id that is present and valid, or a new `UUIDv7`.
    fn resolve(&self, request: &Request) -> Arc<str> {
        if self.trust_incoming {
            if let Some(id) = request
                .headers()
                .get(&self.header)
                .and_then(|value| value.to_str().ok())
                .filter(|value| self.valid_inbound(value))
            {
                return Arc::from(id);
            }
        }

        for source in &self.inbound {
            match source {
                Inbound::TraceParent => {
                    if let Some(id) = request
                        .headers()
                        .get(HeaderName::from_static("traceparent"))
                        .and_then(|value| value.to_str().ok())
                        .and_then(trace_id)
                    {
                        return Arc::from(id);
                    }
                }
                Inbound::Platform => {
                    if let Some(id) = request.extensions().get::<PlatformRequestId>() {
                        return Arc::from(&*id.0);
                    }
                }
            }
        }

        Arc::from(uuid::Uuid::now_v7().to_string())
    }

    /// A reusable inbound id is non-empty visible ASCII within `max_len` — anything longer
    /// or carrying control or high bytes is a spoof vector, not an identifier.
    fn valid_inbound(&self, value: &str) -> bool {
        !value.is_empty()
            && value.len() <= self.max_len
            && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id)
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        &self.id
    }
}

impl Middleware for RequestId {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let id = self.provide(request);
        let mut response = next.run(request).await?;
        self.stamp(&mut response, &id);
        Ok(response)
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<Self>()]
    }
}

http_kit::http_error!(
    /// Raised when a handler extracts [`RequestId`] but no request-id middleware ran.
    ///
    /// Reaching it means the route was served without the wiring
    /// [`Route::build`](crate::routing::Route::build) checks for — a router layered after
    /// the build, or a bare endpoint in a test.
    pub MissingRequestId,
    StatusCode::INTERNAL_SERVER_ERROR,
    "RequestId is not available: no `RequestId` middleware ran for this request; add `.layer(RequestId::new())`"
);

impl Extractor for RequestId {
    type Error = MissingRequestId;

    // Reading the id back out of the extensions is a synchronous clone, so the future is
    // ready on creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or_else(MissingRequestId::new),
        )
    }

    fn requirements() -> Vec<Requirement> {
        vec![Requirement::of::<Self>("`.layer(RequestId::new())`")]
    }
}

/// The `trace-id` segment of a `traceparent` value: `version-traceid-spanid-flags`.
fn trace_id(traceparent: &str) -> Option<&str> {
    let mut segments = traceparent.split('-');
    let version = segments.next()?;
    let trace_id = segments.next()?;
    if version.is_empty()
        || trace_id.len() != 32
        || !trace_id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some(trace_id)
}

#[cfg(test)]
mod tests {
    use super::{trace_id, RequestId};
    use crate::{
        routing::{CreateRouteNode, Route},
        Body, Request, Result, StatusCode,
    };
    use http_kit::HttpError as _;
    use skyzen_core::{Extractor, PlatformRequestId};

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    async fn echo(id: RequestId) -> Result<String> {
        Result::Ok(id.to_string())
    }

    #[tokio::test]
    async fn mints_an_id_the_handler_reads_and_the_response_echoes() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::new())
            .build();

        let response = router.go(get("/echo")).await.unwrap();
        let echoed = response
            .headers()
            .get("x-request-id")
            .expect("the response echoes the id")
            .to_str()
            .unwrap()
            .to_owned();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body.len(), 36, "uuidv7 renders hyphenated: {body}");
        assert_eq!(echoed, body);
    }

    #[tokio::test]
    async fn untrusted_incoming_ids_are_replaced() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::new())
            .build();

        let mut request = get("/echo");
        request
            .headers_mut()
            .insert("x-request-id", "client-chosen".parse().unwrap());
        let response = router.go(request).await.unwrap();
        assert_ne!(
            response.into_body().into_string().await.unwrap(),
            "client-chosen"
        );
    }

    #[tokio::test]
    async fn trusted_incoming_ids_are_reused() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::new().trust_incoming(true))
            .build();

        let mut request = get("/echo");
        request
            .headers_mut()
            .insert("x-request-id", "upstream-id-123".parse().unwrap());
        let response = router.go(request).await.unwrap();
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "upstream-id-123"
        );
    }

    #[tokio::test]
    async fn inbound_ids_past_max_len_are_replaced() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::new().trust_incoming(true).max_len(8))
            .build();

        let mut request = get("/echo");
        request.headers_mut().insert(
            "x-request-id",
            "way-too-long-for-the-limit".parse().unwrap(),
        );
        let response = router.go(request).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body.len(), 36, "a minted uuid replaced the oversized id");
    }

    #[tokio::test]
    async fn cloudflare_preset_reads_the_ray_header() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::cloudflare())
            .build();

        let mut request = get("/echo");
        request
            .headers_mut()
            .insert("cf-ray", "9a8b7c6d5e4f-SJC".parse().unwrap());
        let response = router.go(request).await.unwrap();
        assert_eq!(
            response.headers().get("cf-ray").unwrap().to_str().unwrap(),
            "9a8b7c6d5e4f-SJC"
        );
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "9a8b7c6d5e4f-SJC"
        );
    }

    #[tokio::test]
    async fn azure_falls_back_to_traceparent() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::azure())
            .build();

        let mut request = get("/echo");
        request.headers_mut().insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        let response = router.go(request).await.unwrap();
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[tokio::test]
    async fn lambda_reads_the_platform_request_id() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::lambda())
            .build();

        let mut request = get("/echo");
        request
            .extensions_mut()
            .insert(PlatformRequestId("aws-request-id-42".to_owned()));
        let response = router.go(request).await.unwrap();
        assert_eq!(
            response.into_body().into_string().await.unwrap(),
            "aws-request-id-42"
        );
    }

    #[tokio::test]
    async fn absent_platform_request_id_falls_back_to_minting() {
        let router = Route::new(("/echo".at(echo),))
            .layer(RequestId::lambda())
            .build();

        let response = router.go(get("/echo")).await.unwrap();
        assert_eq!(response.into_body().into_string().await.unwrap().len(), 36);
    }

    #[test]
    fn extracting_without_the_layer_fails_the_build() {
        let error = Route::new(("/echo".at(echo),)).try_build().unwrap_err();
        assert!(
            error.to_string().contains("`.layer(RequestId::new())`"),
            "the error should name the fix: {error}"
        );
    }

    #[tokio::test]
    async fn extraction_without_the_extension_is_a_500() {
        let mut request = Request::new(Body::empty());
        let error = RequestId::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn trace_id_parses_the_middle_segment() {
        assert_eq!(
            trace_id("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        assert_eq!(trace_id("garbage"), None);
        assert_eq!(trace_id("00-nope-00f067aa0ba902b7-01"), None);
    }
}
