//! Connection-level helpers shared by server backends.
//!
//! These are used by every Skyzen HTTP backend (the built-in native runtime and the
//! `skyzen-hyper` adapter) so that peer-address handling and error-to-response conversion are
//! defined exactly once.

use std::net::SocketAddr;

use http_kit::{header, http_error, Body, HttpError, Method, Request, Response, StatusCode};
use serde::Serialize;

use crate::Extractor;

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
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .copied()
            .ok_or(MissingRemoteAddr::new())
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// Renders an error together with its `source()` chain, so one log line carries the whole cause.
struct ErrorChain<'a>(&'a dyn HttpError);

impl core::fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self.0, f)?;
        let mut source = self.0.source();
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

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

    let payload = serde_json::to_vec(&ErrorBody {
        error: &body_message,
    })
    .unwrap_or_else(|_| br#"{"error":"Internal server error"}"#.to_vec());

    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
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
