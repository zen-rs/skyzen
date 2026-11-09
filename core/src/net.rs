//! Connection-level helpers shared by server backends.
//!
//! These are used by every Skyzen HTTP backend (the built-in native runtime and the
//! `skyzen-hyper` adapter) so that peer-address handling and error-to-response conversion are
//! defined exactly once.

use core::future::Future;
use std::net::SocketAddr;

use http_kit::{header, http_error, Body, HttpError, Method, Request, Response, StatusCode};
use serde::Serialize;

use crate::{error::ErrorChain, Extractor};

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

/// Remote socket address of the current connection.
///
/// Server backends insert this into the request extensions for every request; the [`PeerAddr`]
/// extractor (and the higher-level `ClientIp` extractor) read it back. Defining the type here lets
/// any backend populate it without depending on the top-level `skyzen` crate.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl PeerAddr {
    /// Wrap a socket address.
    #[must_use]
    pub const fn new(addr: SocketAddr) -> Self {
        Self(addr)
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
        core::future::ready(
            request
                .extensions()
                .get::<Self>()
                .copied()
                .ok_or(MissingRemoteAddr::new()),
        )
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
            response.headers_mut().append(name.clone(), value.clone());
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
    // offers beyond `status` and `Display`.
    let details = std::iter::successors(error.source(), |source| source.source())
        .find_map(|source| source.downcast_ref::<ErrorDetails>())
        .map(|details| details.0.clone());
    let (payload, content_type) = match details {
        // An error that published machine-readable detail gets the RFC 9457 problem
        // document; the detail object's members merge in as extensions.
        Some(mut details) => {
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
        }
        None => (
            serde_json::to_vec(&ErrorBody {
                error: &body_message,
            })
            .unwrap_or_else(|_| br#"{"error":"Internal server error"}"#.to_vec()),
            "application/json",
        ),
    };

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
