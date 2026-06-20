//! Connection-level helpers shared by server backends.
//!
//! These are used by every Skyzen HTTP backend (the built-in native runtime and the
//! `skyzen-hyper` adapter) so that peer-address handling and error-to-response conversion are
//! defined exactly once.

use std::net::SocketAddr;

use http_kit::{header, http_error, Body, HttpError, Request, Response, StatusCode};
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
