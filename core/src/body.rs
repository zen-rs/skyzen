//! Reading the request body: who is allowed to, and how much of it.
//!
//! A request carries exactly one body, and reading it consumes it. Two rules make that safe:
//!
//! - [`RequestBodyLimit`] caps how many bytes an extractor may buffer, so a large upload cannot
//!   exhaust the server's memory.
//! - [`BodyConsumed`] records which extractor took the body, so a second body-consuming extractor
//!   in the same handler signature fails loudly instead of silently observing an empty body.
//!
//! Both are enforced by [`take_body_bytes`], which every buffering extractor calls, and the
//! consumption half by [`take_body_stream`], which the extractors that hand the stream on call.

use core::any::type_name;
use core::convert::Infallible;
use core::error::Error as StdError;
use core::fmt::{self, Debug, Display};
use core::future::Future;
use core::mem;
use core::pin::pin;

use alloc::vec::Vec;
use http_kit::header::CONTENT_LENGTH;
use http_kit::utils::{Bytes, StreamExt};
use http_kit::{http_error, Body, HttpError, Request, StatusCode};

use crate::Extractor;

/// How many request-body bytes the body extractors may buffer for one request.
///
/// The router inserts this into every request's extensions before dispatch, defaulting to
/// [`RequestBodyLimit::DEFAULT`]. A `BodyLimit` middleware attached to a route (or as a router
/// layer) replaces it for the requests it covers, and body extractors read it back with
/// [`RequestBodyLimit::of`].
///
/// [`take_body_bytes`] enforces it: an oversized payload is rejected with
/// `413 Payload Too Large`, both when `Content-Length` declares the excess up front and when a
/// chunked body only reveals it while streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBodyLimit(Option<usize>);

impl RequestBodyLimit {
    /// The limit applied when nothing overrides it: 2 MiB, matching the ecosystem default.
    pub const DEFAULT: usize = 2 * 1024 * 1024;

    /// Cap bodies at `max_bytes`.
    #[must_use]
    pub const fn new(max_bytes: usize) -> Self {
        Self(Some(max_bytes))
    }

    /// Lift the cap entirely, for endpoints that stream large uploads.
    #[must_use]
    pub const fn disabled() -> Self {
        Self(None)
    }

    /// The cap in bytes, or `None` when the limit is disabled.
    #[must_use]
    pub const fn max_bytes(self) -> Option<usize> {
        self.0
    }

    /// The limit in force for `request`, falling back to the default when none was inserted.
    #[must_use]
    pub fn of(request: &Request) -> Self {
        request
            .extensions()
            .get::<Self>()
            .copied()
            .unwrap_or_default()
    }
}

impl Default for RequestBodyLimit {
    fn default() -> Self {
        Self::new(Self::DEFAULT)
    }
}

/// Reads the limit in force for the current request, so a handler can size its own reads.
impl Extractor for RequestBodyLimit {
    type Error = Infallible;
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        core::future::ready(Ok(Self::of(request)))
    }
}

/// Records that an extractor has already taken this request's body.
///
/// Inserted into the request extensions by [`take_body_bytes`] and [`take_body_stream`] at the
/// moment the body is taken — before it is parsed, so a *failed* body extractor poisons the body
/// just as a successful one does.
#[derive(Debug, Clone, Copy)]
pub struct BodyConsumed(&'static str);

impl BodyConsumed {
    /// The extractor that took the body.
    #[must_use]
    pub const fn by(self) -> &'static str {
        self.0
    }
}

/// Raised when a second body-consuming extractor appears in one handler signature.
///
/// This is a programmer error, not a client error, so it reports `500` and names both extractors.
#[derive(Debug, Clone, Copy)]
pub struct BodyAlreadyConsumed {
    requested: &'static str,
    owner: &'static str,
}

impl BodyAlreadyConsumed {
    /// The extractor that tried to read the body second.
    #[must_use]
    pub const fn requested(&self) -> &'static str {
        self.requested
    }

    /// The extractor that had already taken it.
    #[must_use]
    pub const fn owner(&self) -> &'static str {
        self.owner
    }
}

impl Display for BodyAlreadyConsumed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` cannot read the body: it was already consumed by `{}` earlier in the handler \
             signature",
            self.requested, self.owner
        )
    }
}

impl StdError for BodyAlreadyConsumed {}

impl HttpError for BodyAlreadyConsumed {
    fn status(&self) -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// Raised when a request body exceeds the [`RequestBodyLimit`] in force.
#[derive(Debug, Clone, Copy)]
pub struct PayloadTooLarge {
    limit: usize,
}

impl PayloadTooLarge {
    /// The cap the body exceeded, in bytes.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl Display for PayloadTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Request body exceeds the {} byte limit", self.limit)
    }
}

impl StdError for PayloadTooLarge {}

impl HttpError for PayloadTooLarge {
    fn status(&self) -> StatusCode {
        StatusCode::PAYLOAD_TOO_LARGE
    }
}

http_error!(
    /// Raised when the request body could not be read or decoded.
    pub InvalidBody,
    StatusCode::BAD_REQUEST,
    "Failed to read request body"
);

/// Why a body-consuming extractor could not obtain the request bytes.
#[derive(Debug)]
pub enum BodyReadError {
    /// Another extractor in the same handler signature had already taken the body.
    AlreadyConsumed(BodyAlreadyConsumed),
    /// The body is larger than the limit in force for this request.
    TooLarge(PayloadTooLarge),
    /// The body could not be read from the transport, or was not valid UTF-8.
    Unreadable(InvalidBody),
}

impl Display for BodyReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyConsumed(error) => Display::fmt(error, f),
            Self::TooLarge(error) => Display::fmt(error, f),
            Self::Unreadable(error) => Display::fmt(error, f),
        }
    }
}

impl StdError for BodyReadError {}

impl HttpError for BodyReadError {
    fn status(&self) -> StatusCode {
        match self {
            Self::AlreadyConsumed(error) => error.status(),
            Self::TooLarge(error) => error.status(),
            Self::Unreadable(error) => error.status(),
        }
    }
}

impl From<BodyAlreadyConsumed> for BodyReadError {
    fn from(error: BodyAlreadyConsumed) -> Self {
        Self::AlreadyConsumed(error)
    }
}

impl From<PayloadTooLarge> for BodyReadError {
    fn from(error: PayloadTooLarge) -> Self {
        Self::TooLarge(error)
    }
}

impl From<InvalidBody> for BodyReadError {
    fn from(error: InvalidBody) -> Self {
        Self::Unreadable(error)
    }
}

/// The rejection of an extractor that reads the body and then parses it.
///
/// One type serves every such extractor — `Json<T>`, `Form<T>`, `Multipart` — so the
/// body-availability rules are stated once and each extractor only supplies its own parse error.
#[derive(Debug)]
pub enum BodyExtractorError<E> {
    /// The bytes were never obtained: consumed, oversized, or unreadable.
    Body(BodyReadError),
    /// The bytes were read but could not be parsed into the target type.
    Parse(E),
}

impl<E: Display> Display for BodyExtractorError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Body(error) => Display::fmt(error, f),
            Self::Parse(error) => Display::fmt(error, f),
        }
    }
}

impl<E: StdError + 'static> StdError for BodyExtractorError<E> {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Body(error) => error.source(),
            Self::Parse(error) => error.source(),
        }
    }
}

impl<E: HttpError> HttpError for BodyExtractorError<E> {
    fn status(&self) -> StatusCode {
        match self {
            Self::Body(error) => error.status(),
            Self::Parse(error) => error.status(),
        }
    }
}

impl<E> From<BodyReadError> for BodyExtractorError<E> {
    fn from(error: BodyReadError) -> Self {
        Self::Body(error)
    }
}

impl<E> From<BodyAlreadyConsumed> for BodyExtractorError<E> {
    fn from(error: BodyAlreadyConsumed) -> Self {
        Self::Body(BodyReadError::AlreadyConsumed(error))
    }
}

/// Take the request body on behalf of `T`, refusing a second read.
///
/// The caller receives the stream as it stands and is responsible for whatever it reads from it —
/// no [`RequestBodyLimit`] is applied here, because the point of handing over the stream is to
/// process it incrementally rather than buffer it.
///
/// # Errors
///
/// Returns [`BodyAlreadyConsumed`] when another extractor already took the body.
pub fn take_body_stream<T: ?Sized>(request: &mut Request) -> Result<Body, BodyAlreadyConsumed> {
    if let Some(consumed) = request.extensions().get::<BodyConsumed>() {
        return Err(BodyAlreadyConsumed {
            requested: type_name::<T>(),
            owner: consumed.0,
        });
    }
    request
        .extensions_mut()
        .insert(BodyConsumed(type_name::<T>()));
    Ok(mem::replace(request.body_mut(), Body::empty()))
}

/// Buffer the request body on behalf of `T`, refusing a second read and honouring the limit.
///
/// `Content-Length` is checked first so an oversized upload is refused before a byte of it is
/// read; a body that declares no length (or lies about it) is capped while streaming, so the
/// buffer never grows past the limit either way.
///
/// # Errors
///
/// Returns [`BodyReadError::AlreadyConsumed`] when another extractor already took the body,
/// [`BodyReadError::TooLarge`] when it exceeds the [`RequestBodyLimit`] in force, and
/// [`BodyReadError::Unreadable`] when the transport fails mid-body.
pub async fn take_body_bytes<T: ?Sized>(request: &mut Request) -> Result<Bytes, BodyReadError> {
    let limit = RequestBodyLimit::of(request).max_bytes();
    let declared = declared_length(request);
    // Claim before checking the length so that *every* body-consuming extractor poisons the body,
    // including one that rejects: a handler cannot half-read a body and leave it usable.
    let body = take_body_stream::<T>(request)?;

    if let (Some(limit), Some(declared)) = (limit, declared) {
        if declared > limit {
            return Err(PayloadTooLarge { limit }.into());
        }
    }

    read_capped(body, limit).await
}

/// The body length the request declares, when it declares a usable one.
fn declared_length(request: &Request) -> Option<usize> {
    request
        .headers()
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// Read `body` to the end, giving up as soon as more than `limit` bytes have arrived.
async fn read_capped(body: Body, limit: Option<usize>) -> Result<Bytes, BodyReadError> {
    let Some(limit) = limit else {
        return body
            .into_bytes()
            .await
            .map_err(|_| InvalidBody::new().into());
    };

    // Deliberately not sized from `Content-Length`: a lying header must not let a client
    // pre-allocate for it.
    let mut buffered = Vec::new();
    let mut body = pin!(body);
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| BodyReadError::from(InvalidBody::new()))?;
        if buffered.len() + chunk.len() > limit {
            return Err(PayloadTooLarge { limit }.into());
        }
        buffered.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffered))
}

#[cfg(test)]
mod tests {
    use super::{take_body_bytes, take_body_stream, BodyReadError, RequestBodyLimit};
    use futures_lite::{future::block_on, stream};
    use http_kit::{header::CONTENT_LENGTH, Body, HttpError, Request, StatusCode};

    fn request_with(body: Body) -> Request {
        Request::new(body)
    }

    #[test]
    fn defaults_to_two_mebibytes() {
        let request = request_with(Body::empty());
        assert_eq!(
            RequestBodyLimit::of(&request).max_bytes(),
            Some(RequestBodyLimit::DEFAULT)
        );
    }

    #[test]
    fn reads_back_an_inserted_override() {
        let mut request = request_with(Body::empty());
        request
            .extensions_mut()
            .insert(RequestBodyLimit::disabled());
        assert_eq!(RequestBodyLimit::of(&request).max_bytes(), None);
    }

    #[test]
    fn a_declared_length_over_the_limit_is_refused_before_reading() {
        let mut request = request_with(Body::from_bytes(vec![0; 64]));
        request.extensions_mut().insert(RequestBodyLimit::new(8));
        request
            .headers_mut()
            .insert(CONTENT_LENGTH, "64".parse().expect("valid header"));

        let error = block_on(take_body_bytes::<()>(&mut request)).unwrap_err();
        assert!(matches!(error, BodyReadError::TooLarge(_)));
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn a_streaming_body_is_capped_without_a_declared_length() {
        // Three chunks of four bytes, no Content-Length: only the running cap can catch this.
        let chunks = stream::iter((0..3).map(|_| Ok::<_, http_kit::BodyError>("abcd")));
        let mut request = request_with(Body::from_stream(chunks));
        request.extensions_mut().insert(RequestBodyLimit::new(8));
        assert_eq!(request.body().len(), None, "the body must look unbounded");

        let error = block_on(take_body_bytes::<()>(&mut request)).unwrap_err();
        assert!(matches!(error, BodyReadError::TooLarge(_)));
    }

    #[test]
    fn a_body_within_the_limit_reads_whole() {
        let mut request = request_with(Body::from_bytes("hello"));
        request.extensions_mut().insert(RequestBodyLimit::new(8));
        let bytes = block_on(take_body_bytes::<()>(&mut request)).expect("within the limit");
        assert_eq!(bytes.as_ref(), b"hello");
    }

    #[test]
    fn a_second_read_names_both_extractors() {
        let mut request = request_with(Body::from_bytes("hello"));
        block_on(take_body_bytes::<u8>(&mut request)).expect("first read succeeds");

        let error = block_on(take_body_bytes::<u16>(&mut request)).unwrap_err();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let message = error.to_string();
        assert!(
            message.contains("u16"),
            "names the second reader: {message}"
        );
        assert!(message.contains("u8"), "names the first reader: {message}");
    }

    #[test]
    fn a_rejected_read_still_poisons_the_body() {
        let mut request = request_with(Body::from_bytes(vec![0; 64]));
        request.extensions_mut().insert(RequestBodyLimit::new(8));
        block_on(take_body_bytes::<u8>(&mut request)).expect_err("over the limit");

        let error = take_body_stream::<u16>(&mut request).unwrap_err();
        assert_eq!(error.owner(), "u8");
    }
}
