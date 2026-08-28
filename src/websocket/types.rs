//! Shared websocket types exposed by the public API without leaking backend dependencies.
use std::{
    fmt,
    future::{ready, Future},
    io,
};

use http_kit::header::{HeaderMap, HeaderValue, SEC_WEBSOCKET_PROTOCOL};
use skyzen_core::Extractor;

/// Result type used by websocket operations.
pub type WebSocketResult<T> = Result<T, WebSocketError>;

/// Close frame representation that avoids depending on tungstenite types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSocketCloseFrame {
    /// Close code sent to peer.
    pub code: u16,
    /// Human readable close reason.
    pub reason: String,
}

impl WebSocketCloseFrame {
    /// Build a close frame from code and reason.
    pub fn new(code: u16, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

/// Errors produced by websocket operations.
#[derive(Debug)]
pub enum WebSocketError {
    /// Underlying IO/transport failure.
    Transport(io::Error),
    /// Protocol-level failure.
    Protocol(String),
    /// An outbound message exceeds the configured maximum message size.
    MessageTooLarge {
        /// Size of the rejected message, in bytes.
        len: usize,
        /// Configured maximum message size, in bytes.
        limit: usize,
    },
}

impl From<io::Error> for WebSocketError {
    fn from(error: io::Error) -> Self {
        Self::Transport(error)
    }
}

impl fmt::Display for WebSocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(err) => write!(f, "transport error: {err}"),
            Self::Protocol(err) => write!(f, "protocol error: {err}"),
            Self::MessageTooLarge { len, limit } => write!(
                f,
                "message of {len} bytes exceeds the configured maximum of {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for WebSocketError {}

// A failed frame is the connection's problem, not the client request's: nothing the peer sent
// makes a transport failure its fault, so the default `500` is the honest status. Implementing
// `HttpError` is also what lets a websocket failure travel through `?` — both into a handler's
// [`skyzen::Result`](crate::Result) and into the session outcome a `.ws` handler returns.
impl http_kit::HttpError for WebSocketError {}

#[cfg(feature = "json")]
impl From<serde_json::Error> for WebSocketError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(error.to_string())
    }
}

/// The subprotocols the client offered, in the order it offered them.
///
/// Each token is a trimmed slice of the request's own `Sec-WebSocket-Protocol`, so anything
/// selected out of this list is by construction a value the handshake response can echo back.
// `websocket::mod` re-exports this module with a glob, so `pub` on these would put the handshake
// internals in the public API; `pub(crate)` is load-bearing rather than redundant.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn offered_protocols(headers: &HeaderMap) -> Vec<String> {
    headers
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(|protocol| protocol.trim().to_owned())
                .filter(|protocol| !protocol.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The first subprotocol the client offered that the handler also supports.
///
/// Answering a subprotocol request is not optional politeness: RFC 6455 §4.1 has the client
/// **fail the connection** when its offer goes unanswered in the `101`. A browser `WebSocket`
/// sends no custom headers, so the subprotocol list is its only in-band credential channel, and a
/// handshake that drops the echo does not degrade the socket — it never opens it.
// `websocket::mod` re-exports this module with a glob, so `pub` on these would put the handshake
// internals in the public API; `pub(crate)` is load-bearing rather than redundant.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn select_protocol(offered: &[String], supported: &[String]) -> Option<HeaderValue> {
    let selected = offered
        .iter()
        .find(|protocol| supported.contains(protocol))?;
    // The token came out of a valid header value, so this cannot fail; and a token that could not
    // be sent back is not a token the server is able to answer with either.
    HeaderValue::from_str(selected).ok()
}

/// The answer to a request's subprotocol offer, given what the handler supports.
///
/// [`offered_protocols`] and [`select_protocol`] in one step, for callers that hold the request
/// rather than a parsed offer.
// `websocket::mod` re-exports this module with a glob, so `pub` on these would put the handshake
// internals in the public API; `pub(crate)` is load-bearing rather than redundant.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn select_offered_protocol(
    headers: &HeaderMap,
    supported: &[String],
) -> Option<HeaderValue> {
    select_protocol(&offered_protocols(headers), supported)
}

/// The subprotocols the client offered, extracted straight from the handshake request.
///
/// A browser cannot put a header on a `WebSocket`, so the subprotocol list is the only thing it
/// can send that the server chooses — which makes it the standard channel for a credential
/// (`new WebSocket(url, ["app.bearer." + token])`). Reading it needs the request, and a
/// [`HibernationWebSocketUpgrade`](crate::durable::HibernationWebSocketUpgrade) is constructed
/// rather than extracted, so the offer arrives as its own extractor:
///
/// ```ignore
/// async fn join(offered: RequestedSubprotocols) -> Result<HibernationWebSocketUpgrade, AuthError> {
///     let token = offered
///         .iter()
///         .find_map(|protocol| protocol.strip_prefix("app.bearer."))
///         .ok_or(AuthError::MissingToken)?;
///     verify(token)?;
///
///     // Answering is not optional: RFC 6455 §4.1 has the client fail the connection when its
///     // offer goes unanswered, so the accepted token is echoed back verbatim.
///     let answer = offered.answer(|protocol| protocol.starts_with("app.bearer."));
///     Ok(HibernationWebSocketUpgrade::new().tag("room").protocol(answer.unwrap()))
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct RequestedSubprotocols(Vec<String>);

impl RequestedSubprotocols {
    /// The offered subprotocols, in the order the client offered them.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// The offered subprotocols as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// The first offered subprotocol `accept` returns `true` for, ready to answer the handshake
    /// with.
    ///
    /// Returns the [`HeaderValue`] that
    /// [`protocol`](crate::durable::HibernationWebSocketUpgrade::protocol) takes, so a handler
    /// echoing back a token it just accepted never has to parse a header value itself.
    #[must_use]
    pub fn answer(&self, mut accept: impl FnMut(&str) -> bool) -> Option<HeaderValue> {
        let selected = self.0.iter().find(|protocol| accept(protocol))?;
        // Every token here is a slice of the request's own header value, so this cannot fail.
        HeaderValue::from_str(selected).ok()
    }
}

impl Extractor for RequestedSubprotocols {
    // A client that offered nothing is not an error, just a client with no offer.
    type Error = core::convert::Infallible;

    // Reading and splitting a header is synchronous, so the future is ready on creation.
    fn extract(
        request: &mut crate::Request,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(Ok(Self(offered_protocols(request.headers()))))
    }
}
