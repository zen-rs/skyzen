//! A per-request id, minted by a layer and readable as an extractor.
//!
//! [`RequestId::new`] builds the layer, which mints a time-ordered `UUIDv7` for every request,
//! stores it in the request extensions and echoes it on the response. Handlers read it back as
//! `id: RequestId`; a route that extracts one without the layer fails at
//! [`Route::build`](crate::routing::Route::build) rather than with a runtime `500`.

use core::fmt;
use std::{any::TypeId, future::ready, ops::Deref};

use super::error_handling::{DeferredHeaders, HandlesErrors};
use super::trace::Trace;
use http_kit::{
    header::{HeaderName, HeaderValue},
    Request, Response,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, Requirement,
};

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
const CF_RAY: HeaderName = HeaderName::from_static("cf-ray");
const AWS_REQUEST_ID: HeaderName = HeaderName::from_static("x-amzn-requestid");
const AZURE_REQUEST_ID: HeaderName = HeaderName::from_static("x-ms-request-id");

/// The id assigned to the request being handled.
///
/// Minted by [`RequestId::new`] (or a platform preset) and inserted into the request extensions;
/// extracted by handlers as `id: RequestId`. Renders as the bare id string through [`Display`],
/// [`AsRef<str>`] and [`Deref`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Build the layer that mints ids, with the default header `x-request-id`.
    ///
    /// ```rust
    /// use skyzen::{middleware::RequestId, routing::{CreateRouteNode, Route}};
    ///
    /// let router = Route::new(("/ping".at(|| async { "pong" }),))
    ///     .layer(RequestId::new())
    ///     .build();
    /// ```
    /// `new` deliberately returns the layer, not `Self`: `RequestId` is the extractor name,
    /// and `RequestId::new()` building the layer keeps `id: RequestId` and
    /// `.layer(RequestId::new())` under one import.
    #[allow(clippy::new_ret_no_self)]
    #[must_use]
    pub fn new() -> RequestIdLayer {
        RequestIdLayer::default()
    }

    /// Reuse Cloudflare's `cf-ray` as the request id — the id the edge already logs against.
    #[must_use]
    pub fn cloudflare() -> RequestIdLayer {
        Self::new().header(CF_RAY).trust_incoming(true)
    }

    /// Reuse the Lambda invocation's request id, which the `skyzen-lambda` adapter records from
    /// the invocation context. The `x-amzn-requestid` header a gateway forwards is the fallback,
    /// then a minted id.
    #[must_use]
    pub fn lambda() -> RequestIdLayer {
        Self::new()
            .header(AWS_REQUEST_ID)
            .trust_incoming(true)
            .platform(true)
    }

    /// Reuse Azure's `x-ms-request-id`.
    #[must_use]
    pub fn azure() -> RequestIdLayer {
        Self::new().header(AZURE_REQUEST_ID).trust_incoming(true)
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Deref for RequestId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

http_kit::http_error!(
    /// A handler extracted [`RequestId`] but no layer minted one.
    pub MissingRequestId,
    http_kit::StatusCode::INTERNAL_SERVER_ERROR,
    "no `RequestId` in the request extensions; add `.layer(RequestId::new())`"
);

impl Extractor for RequestId {
    type Error = MissingRequestId;
    fn extract(
        request: &mut Request,
    ) -> impl core::future::Future<Output = Result<Self, Self::Error>> + Send {
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

/// The layer [`RequestId::new`] builds: mints or reuses an id per request.
///
/// The id is a `UUIDv7`, so logs ordered by it are also ordered in time. An inbound id is reused
/// only with [`trust_incoming`](Self::trust_incoming) — a client-chosen id is a log-injection
/// vector unless a trusted proxy sets it.
#[derive(Debug, Clone)]
pub struct RequestIdLayer {
    header: HeaderName,
    trust_incoming: bool,
    max_len: usize,
    /// Reuse the platform's own invocation id (`skyzen_core::PlatformRequestId`) when the
    /// adapter recorded one — only the `lambda()` preset sets this.
    platform: bool,
    inner: Option<Trace>,
}

impl Default for RequestIdLayer {
    fn default() -> Self {
        Self {
            header: X_REQUEST_ID,
            trust_incoming: false,
            max_len: 128,
            platform: false,
            inner: None,
        }
    }
}

impl RequestIdLayer {
    /// The header read and echoed. Defaults to `x-request-id`.
    #[must_use]
    pub fn header(mut self, header: HeaderName) -> Self {
        self.header = header;
        self
    }

    /// Reuse the inbound header value instead of minting a new id. Off by default.
    #[must_use]
    pub const fn trust_incoming(mut self, trust: bool) -> Self {
        self.trust_incoming = trust;
        self
    }

    /// The longest inbound id accepted. Longer or non-visible-ASCII values are replaced.
    #[must_use]
    pub const fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self
    }

    /// Prefer the hosting platform's invocation id over both a minted id and the inbound header.
    const fn platform(mut self, platform: bool) -> Self {
        self.platform = platform;
        self
    }

    /// The `Trace` layer this id was bundled with by [`Trace::with_request_id`].
    pub(crate) fn with_trace(mut self, trace: Trace) -> Self {
        self.inner = Some(trace);
        self
    }
}

/// Whether an inbound header value is safe to reuse: visible ASCII within the length cap.
fn valid_incoming(value: &HeaderValue, max_len: usize) -> Option<String> {
    let raw = value.to_str().ok()?;
    (raw.len() <= max_len && !raw.is_empty() && raw.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        .then(|| raw.to_owned())
}

impl Middleware for RequestIdLayer {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let platform_id = self
            .platform
            .then(|| {
                request
                    .extensions()
                    .get::<skyzen_core::PlatformRequestId>()
                    .map(|id| id.to_string())
            })
            .flatten();
        let id = platform_id.unwrap_or_else(|| {
            if self.trust_incoming {
                request
                    .headers()
                    .get(&self.header)
                    .and_then(|value| valid_incoming(value, self.max_len))
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string())
            } else {
                uuid::Uuid::now_v7().to_string()
            }
        });
        let value = HeaderValue::from_str(&id)
            .map_err(|_| Error::msg("generated request id is not a header value"))?;
        request.extensions_mut().insert(RequestId(id));
        // So an ErrorHandlingMiddleware's response still carries the id.
        request
            .extensions_mut()
            .insert(DeferredHeaders(vec![(self.header.clone(), value.clone())]));

        let result = match &self.inner {
            Some(trace) => trace.wrap(request, next).await,
            None => next.run(request).await,
        };

        let mut response = match result {
            Ok(response) => response,
            // Rendering the error inside the layer is what lets the id reach a response the
            // server edge would otherwise build without it — unless an
            // `ErrorHandlingMiddleware` already claims the error path.
            Err(error) if request.extensions().get::<HandlesErrors>().is_some() => {
                return Err(error);
            }
            Err(error) => skyzen_core::error_response(&*error.into_boxed_http_error()),
        };
        response.headers_mut().insert(self.header.clone(), value);
        Ok(response)
    }

    fn provisions(&self) -> Vec<TypeId> {
        let mut provisions = vec![TypeId::of::<RequestId>()];
        if let Some(trace) = &self.inner {
            provisions.extend(trace.provisions());
        }
        provisions
    }
}

#[cfg(test)]
mod tests {
    use super::RequestId;
    use crate::{
        middleware::Middleware,
        routing::{CreateRouteNode, Route, RouteBuildError},
        Body, Request, Result,
    };
    use http_kit::header::HeaderValue;

    async fn echo_id(id: RequestId) -> Result<String> {
        Ok(id.to_string())
    }

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().unwrap();
        request
    }

    #[tokio::test]
    async fn mints_and_echoes_an_id() {
        let router = Route::new(("/ping".at(echo_id),))
            .layer(RequestId::new())
            .build();
        let response = router.go(get("/ping")).await.unwrap();
        let echoed = response
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(echoed.len(), 36);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, echoed);
    }

    #[tokio::test]
    async fn untrusted_incoming_id_is_replaced() {
        let router = Route::new(("/ping".at(echo_id),))
            .layer(RequestId::new())
            .build();
        let mut request = get("/ping");
        request
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_static("attacker-chosen"));

        let response = router.go(request).await.unwrap();
        assert_ne!(
            response.headers().get("x-request-id").unwrap(),
            "attacker-chosen"
        );
    }

    #[tokio::test]
    async fn trusted_incoming_id_is_reused() {
        let router = Route::new(("/ping".at(echo_id),))
            .layer(RequestId::new().trust_incoming(true))
            .build();
        let mut request = get("/ping");
        request
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_static("proxy-set-id"));

        let response = router.go(request).await.unwrap();
        assert_eq!(
            response.headers().get("x-request-id").unwrap(),
            "proxy-set-id"
        );
    }

    #[tokio::test]
    async fn trusted_but_oversized_or_invisible_id_is_replaced() {
        let router = Route::new(("/ping".at(echo_id),))
            .layer(RequestId::new().trust_incoming(true).max_len(8))
            .build();
        let mut request = get("/ping");
        request
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_static("way-too-long-id"));

        let response = router.go(request).await.unwrap();
        let id = response.headers().get("x-request-id").unwrap();
        assert_eq!(id.to_str().unwrap().len(), 36);
    }

    #[test]
    fn extracting_without_the_layer_fails_at_build() {
        let error = Route::new(("/ping".at(echo_id),)).try_build().unwrap_err();
        assert!(matches!(error, RouteBuildError::MissingProvision { .. }));
    }

    #[test]
    fn the_layer_declares_itself_a_provider() {
        let layer = RequestId::new();
        assert!(Middleware::provisions(&layer).contains(&std::any::TypeId::of::<RequestId>()));
    }

    #[tokio::test]
    async fn an_id_is_echoed_on_error_responses() {
        async fn boom() -> Result<String> {
            Err(crate::Error::msg("boom"))
        }
        let router = Route::new(("/boom".at(boom),))
            .layer(RequestId::new())
            .build();
        let response = router.go(get("/boom")).await.unwrap();
        assert_eq!(response.status(), crate::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().contains_key("x-request-id"));
    }
}
