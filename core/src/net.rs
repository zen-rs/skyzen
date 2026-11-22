//! Connection-level helpers shared by server backends.
//!
//! These are used by every Skyzen HTTP backend (the built-in native runtime and the
//! `skyzen-hyper` adapter) so that peer-address handling and error-to-response conversion are
//! defined exactly once.

use core::{future::Future, time::Duration};
use std::net::SocketAddr;

use http_kit::{header, http_error, Body, HttpError, Method, Request, Response, StatusCode};
use serde::Serialize;

use crate::{error::ErrorChain, Extractor};

/// An `Alt-Svc` value (RFC 7838) announcing an HTTP/3 endpoint.
///
/// A server that terminates TLS on TCP and serves HTTP/3 on UDP attaches it to its TCP responses
/// so a client that reached it over HTTP/1.1 or HTTP/2 learns it can switch. Clients only honour
/// the header over a secure connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltSvc {
    port: u16,
    max_age: Duration,
}

impl AltSvc {
    /// How long a client may remember the alternative unless [`max_age`](Self::max_age) says
    /// otherwise: the 24 hours RFC 7838 §3.1 names as the default.
    pub const DEFAULT_MAX_AGE: Duration = Duration::from_hours(24);

    /// HTTP/3 on UDP `port` of the same host.
    #[must_use]
    pub const fn h3(port: u16) -> Self {
        Self {
            port,
            max_age: Self::DEFAULT_MAX_AGE,
        }
    }

    /// Let clients remember the alternative for `max_age`.
    #[must_use]
    pub const fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// The `Alt-Svc` header value this announces, for a server that attaches it by itself.
    ///
    /// # Panics
    ///
    /// Never: the value is visible ASCII by construction.
    #[must_use]
    pub fn header_value(&self) -> header::HeaderValue {
        header::HeaderValue::from_str(&alloc::format!(
            "h3=\":{}\"; ma={}",
            self.port,
            self.max_age.as_secs()
        ))
        .expect("an Alt-Svc value is visible ASCII")
    }
}

/// The message a panicking task carried, when it carried a printable one.
///
/// Every backend that runs application code behind a catch — the native queue-consumer driver and
/// the Lambda adapter both do, so that one poisonous message cannot take a whole runtime down —
/// needs this, and the standard library offers no way to ask a panic payload for its text.
#[must_use]
pub fn panic_message(panic: &(dyn core::any::Any + Send)) -> &str {
    panic.downcast_ref::<&'static str>().map_or_else(
        || {
            panic
                .downcast_ref::<alloc::string::String>()
                .map_or("<non-string panic payload>", alloc::string::String::as_str)
        },
        |message| *message,
    )
}

http_error!(
    /// Raised when the connection metadata does not expose the remote address.
    pub MissingRemoteAddr,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Missing remote addr, maybe it's not a tcp/udp connection"
);

/// Per-connection metadata attached in a single extension slot by an HTTP server backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectionInfo {
    peer: Option<PeerAddr>,
    upgrade_available: bool,
}

impl ConnectionInfo {
    /// Metadata for a connection with optional `peer` and HTTP/1.1 upgrade capability.
    #[must_use]
    pub const fn new(peer: Option<SocketAddr>, upgrade_available: bool) -> Self {
        Self {
            peer: match peer {
                Some(addr) => Some(PeerAddr::new(addr)),
                None => None,
            },
            upgrade_available,
        }
    }

    /// Remote socket address, if the transport exposes one.
    #[must_use]
    pub const fn peer(&self) -> Option<PeerAddr> {
        self.peer
    }

    /// Whether the server can hand the connection over after a `101` response.
    #[must_use]
    pub const fn upgrade_available(&self) -> bool {
        self.upgrade_available
    }
}

/// Remote socket address of the current connection.
///
/// Server backends record this in [`ConnectionInfo`] (or directly in request extensions in tests);
/// [`PeerAddr::of`], the [`PeerAddr`] extractor, and `ClientIp` read it back.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl PeerAddr {
    /// Wrap a socket address.
    #[must_use]
    pub const fn new(addr: SocketAddr) -> Self {
        Self(addr)
    }

    /// Read the connection's remote address from `request`.
    #[must_use]
    pub fn of(request: &Request) -> Option<Self> {
        let extensions = request.extensions();
        extensions
            .get::<ConnectionInfo>()
            .and_then(ConnectionInfo::peer)
            .or_else(|| extensions.get::<Self>().copied())
    }
}

/// The peer address an `http-kit-io` [`Listener`](http_kit_io::Listener) reports, as the socket
/// address a request's [`PeerAddr`] carries.
///
/// A TCP peer has one. A unix-domain peer is a path, or nothing, and has none: requests on its
/// connections carry no [`PeerAddr`].
pub trait PeerAddress {
    /// The peer's socket address, when it has one.
    fn socket_addr(&self) -> Option<SocketAddr>;
}

impl PeerAddress for SocketAddr {
    fn socket_addr(&self) -> Option<SocketAddr> {
        Some(*self)
    }
}

#[cfg(unix)]
impl PeerAddress for std::os::unix::net::SocketAddr {
    fn socket_addr(&self) -> Option<SocketAddr> {
        None
    }
}

impl core::ops::Deref for PeerAddr {
    type Target = SocketAddr;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Extractor for PeerAddr {
    type Error = MissingRemoteAddr;
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        core::future::ready(Self::of(request).ok_or(MissingRemoteAddr::new()))
    }
}

/// The identifier the hosting platform assigned to this invocation.
///
/// Serverless adapters insert it from the invocation context before the endpoint runs —
/// `skyzen-lambda` copies `lambda_runtime::Context::request_id` — so a request-id layer can
/// reuse the platform's own identifier instead of minting one no trace will ever join back
/// to the invocation. Defining the type here lets any adapter populate it without depending
/// on the top-level `skyzen` crate.
#[derive(Debug, Clone)]
pub struct PlatformRequestId(pub alloc::string::String);

impl core::ops::Deref for PlatformRequestId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Response headers that must survive an endpoint error.
///
/// Middleware like `skyzen::middleware::RequestId` puts headers here when the client needs
/// them even when the endpoint returns `Err` — a request id is the motivating case. The
/// backend renders an `Err` into a fresh response with [`error_response`], outside any
/// middleware's reach, so the headers travel on the request instead and every backend merges
/// them at its error boundary with [`apply_pending_headers`].
#[derive(Debug, Default, Clone)]
pub struct PendingResponseHeaders(pub header::HeaderMap);

/// Copy [`PendingResponseHeaders`] from `request` onto `response`.
///
/// Backends call this where they turn an endpoint `Err` into a response — alongside
/// [`error_response`] — so headers middleware marked as must-deliver survive errors.
pub fn apply_pending_headers(request: &Request, response: &mut Response) {
    if let Some(pending) = request.extensions().get::<PendingResponseHeaders>() {
        for (name, value) in &pending.0 {
            // Multi-value headers (`Set-Cookie`) must append — each carries its own
            // directive and a first-value-wins rule would silently drop one. Everything
            // else is a default: a response that set its own wins.
            if *name == header::SET_COOKIE {
                response.headers_mut().append(name.clone(), value.clone());
            } else {
                response
                    .headers_mut()
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// Machine-readable detail an error carries in its `source()` chain.
///
/// [`error_response`] upgrades a body carrying one to an RFC 9457 problem document
/// (`application/problem+json`), merging the value's members in as extensions. An error
/// reports one by storing it as its `source()`; it is not itself an `HttpError`, so it never
/// sets the status.
#[derive(Debug)]
pub struct ErrorDetails(pub serde_json::Value);

impl core::fmt::Display for ErrorDetails {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("error details")
    }
}

impl core::error::Error for ErrorDetails {}

/// Emit the endpoint-error log line that every Skyzen backend shares.
///
/// Server (5xx) errors log at `error`, everything else at `warn`, and both carry the request's
/// method and path plus the error's full `source()` chain — the chain is the only place the
/// detail survives for a 5xx, whose response body is redacted.
///
/// Backends call this immediately before [`error_response`] so the operator-facing record and the
/// client-facing body are produced by the same policy in every runtime.
pub fn log_endpoint_error(error: &dyn HttpError, method: &Method, path: &str) {
    let status = error.status();
    let chain = ErrorChain(error);
    if status.is_server_error() {
        tracing::error!(
            method = method.as_str(),
            path,
            status = status.as_u16(),
            error = %chain,
            "internal server error"
        );
    } else {
        tracing::warn!(
            method = method.as_str(),
            path,
            status = status.as_u16(),
            error = %chain,
            "client error"
        );
    }
}

/// Convert an endpoint error into an HTTP response carrying a JSON body.
///
/// Server (5xx) errors hide their internal detail behind a generic message to avoid leaking
/// implementation specifics; client (4xx) and other errors surface the error's display message.
/// The body is serialized with `serde_json` rather than ad-hoc string formatting, so control
/// characters or quotes in the message can never produce malformed JSON.
#[must_use]
pub fn error_response(error: &dyn HttpError) -> Response {
    let status = error.status();
    let body_message = if status.is_server_error() {
        "Internal server error".to_owned()
    } else {
        error.to_string()
    };

    // Structured detail rides in the `source()` chain — the only channel `dyn HttpError`
    // offers beyond `status` and `Display`. Server errors keep the same silence they have
    // always kept: a detail node could expose internals the redaction exists to hide.
    let details = (!status.is_server_error())
        .then(|| {
            std::iter::successors(error.source(), |source| source.source())
                .find_map(|source| source.downcast_ref::<ErrorDetails>())
                .map(|details| details.0.clone())
        })
        .flatten();
    let (payload, content_type) = details.map_or_else(
        || {
            (
                serde_json::to_vec(&ErrorBody {
                    error: &body_message,
                })
                .unwrap_or_else(|_| br#"{"error":"Internal server error"}"#.to_vec()),
                "application/json",
            )
        },
        // An error that published machine-readable detail gets the RFC 9457 problem
        // document; the detail object's members merge in as extensions.
        |mut details| {
            let mut problem = serde_json::json!({
                "type": "about:blank",
                "title": status.canonical_reason(),
                "status": status.as_u16(),
                "detail": body_message,
            });
            if let (Some(problem), Some(details)) =
                (problem.as_object_mut(), details.as_object_mut())
            {
                problem.append(details);
            }
            (
                serde_json::to_vec(&problem).unwrap_or_else(|_| br#"{"title":"error"}"#.to_vec()),
                "application/problem+json",
            )
        },
    );

    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::error_response;
    use http_kit::{header, http_error, StatusCode};

    http_error!(
        /// Client error carrying a user-facing message.
        TeapotError,
        StatusCode::IM_A_TEAPOT,
        "the teapot is busy"
    );

    http_error!(
        /// Server error carrying an internal detail that must not leak.
        SecretDbError,
        StatusCode::INTERNAL_SERVER_ERROR,
        "db password is hunter2"
    );

    fn body_json(response: crate::Response) -> serde_json::Value {
        let bytes =
            futures_lite::future::block_on(response.into_body().into_bytes()).expect("read body");
        serde_json::from_slice(&bytes).expect("body is valid JSON")
    }

    #[test]
    fn client_error_keeps_message() {
        let response = error_response(&TeapotError::new());
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(
            body_json(response),
            serde_json::json!({ "error": "the teapot is busy" })
        );
    }

    #[test]
    fn server_error_is_redacted() {
        let response = error_response(&SecretDbError::new());
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(response),
            serde_json::json!({ "error": "Internal server error" })
        );
    }

    #[test]
    fn error_response_sets_json_content_type() {
        let response = error_response(&TeapotError::new());
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }
}
