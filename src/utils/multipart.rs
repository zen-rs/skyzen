//! Multipart form data utilities module.
//! It provides an extractor for `multipart/form-data` requests.

use core::future::{ready, Future};
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::{
    extract::Extractor,
    header::{HeaderMap, CONTENT_TYPE},
    middleware::{Middleware, Next},
    Body, Error, Request, Response, StatusCode,
};
use futures_core::Stream;
use http_kit::utils::{Bytes, Stream as LiteStream};
use http_kit::{BodyError, DataStream};
use multer::Field as MulterField;
use pin_project_lite::pin_project;
use skyzen_core::{take_body_stream, BodyExtractorError, RequestBodyLimit};

/// Default maximum number of fields accepted in a single multipart request: 64.
///
/// Prevents field-flooding denial-of-service attacks (thousands of tiny parts) while covering
/// typical HTML forms and multi-file uploads.
pub const DEFAULT_MAX_FIELDS: usize = 64;

/// Default maximum size in bytes for a single multipart field: 2 MiB.
///
/// Matches [`RequestBodyLimit::DEFAULT`] so an individual field cannot grow unboundedly even when
/// the whole-stream limit is disabled via [`BodyLimit::disabled`](crate::middleware::BodyLimit::disabled).
pub const DEFAULT_MAX_FIELD_SIZE: usize = RequestBodyLimit::DEFAULT;

/// Override the per-field size and field-count limits enforced by [`Multipart`].
///
/// By default, [`Multipart`] caps requests at [`DEFAULT_MAX_FIELDS`] (64) fields, caps the whole
/// stream at [`RequestBodyLimit`], and caps each individual field at [`RequestBodyLimit`] (or
/// [`DEFAULT_MAX_FIELD_SIZE`] when `RequestBodyLimit` is disabled). Attach this middleware to a
/// route or router to tighten, raise, or lift those per-field limits:
///
/// ```rust
/// use skyzen::{middleware::MultipartLimits, routing::{CreateRouteNode, Route}, Result};
///
/// let route = Route::new((
///     "/upload".post(|| async { Result::Ok("stored") }),
/// ))
/// .with(MultipartLimits::new().max_fields(Some(16)).max_field_size(Some(8 * 1024 * 1024)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipartLimits {
    max_fields: Option<usize>,
    max_field_size: Option<usize>,
}

impl Default for MultipartLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl MultipartLimits {
    /// Create multipart limits with the hardened defaults ([`DEFAULT_MAX_FIELDS`] and
    /// [`DEFAULT_MAX_FIELD_SIZE`]).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_fields: Some(DEFAULT_MAX_FIELDS),
            max_field_size: Some(DEFAULT_MAX_FIELD_SIZE),
        }
    }

    /// Set the maximum number of fields accepted in a single multipart request.
    ///
    /// Pass `None` (or call [`without_field_count_limit`](Self::without_field_count_limit)) to
    /// allow an unlimited number of fields.
    #[must_use]
    pub const fn max_fields(mut self, max_fields: Option<usize>) -> Self {
        self.max_fields = max_fields;
        self
    }

    /// Disable the maximum field-count limit.
    #[must_use]
    pub const fn without_field_count_limit(mut self) -> Self {
        self.max_fields = None;
        self
    }

    /// Set the maximum size in bytes for any single multipart field.
    ///
    /// Pass `None` (or call [`without_field_size_limit`](Self::without_field_size_limit)) to
    /// disable the per-field limit (the whole-stream [`RequestBodyLimit`] still applies).
    #[must_use]
    pub const fn max_field_size(mut self, max_field_size: Option<usize>) -> Self {
        self.max_field_size = max_field_size;
        self
    }

    /// Disable the per-field size limit.
    #[must_use]
    pub const fn without_field_size_limit(mut self) -> Self {
        self.max_field_size = None;
        self
    }
}

impl Middleware for MultipartLimits {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        request.extensions_mut().insert(*self);
        next.run(request).await
    }
}

/// Extractor that parses `multipart/form-data` bodies.
#[derive(Debug)]
pub struct Multipart {
    inner: multer::Multipart<'static>,
    fields_seen: usize,
    max_fields: Option<usize>,
}

/// What a [`Multipart`] extraction can fail with: the body was unavailable, or it carried no
/// usable boundary.
pub type MultipartRejection = BodyExtractorError<MultipartBoundaryError>;

impl Multipart {
    fn from_parts(
        boundary: String,
        body: Body,
        limit: RequestBodyLimit,
        limits: Option<MultipartLimits>,
    ) -> Self {
        let stream = RequestBodyStream::new(body);
        let whole_stream = limit.max_bytes();
        // Follow a raised `BodyLimit::max(...)` automatically while still keeping a finite
        // per-field cap if the whole-stream limit was disabled.
        let per_field = limits.map_or_else(
            || whole_stream.or(Some(DEFAULT_MAX_FIELD_SIZE)),
            |custom| custom.max_field_size,
        );
        let max_fields = limits.map_or(Some(DEFAULT_MAX_FIELDS), |custom| custom.max_fields);

        let mut size_limit = multer::SizeLimit::new();
        if let Some(max_bytes) = whole_stream {
            size_limit = size_limit.whole_stream(max_bytes as u64);
        }
        if let Some(max_field_bytes) = per_field {
            size_limit = size_limit.per_field(max_field_bytes as u64);
        }

        let inner = multer::Multipart::with_constraints(
            stream,
            boundary,
            multer::Constraints::new().size_limit(size_limit),
        );
        Self {
            inner,
            fields_seen: 0,
            max_fields,
        }
    }

    /// Yields the next [`Field`] if available.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError`] if parsing the field fails or the configured limits are exceeded.
    pub async fn next_field(&mut self) -> Result<Option<Field<'_>>, MultipartError> {
        let field = self
            .inner
            .next_field()
            .await
            .map_err(MultipartError::from_multer)?;

        if let Some(inner) = field {
            if let Some(limit) = self.max_fields {
                if self.fields_seen >= limit {
                    return Err(MultipartError::too_many_fields(limit));
                }
            }
            self.fields_seen += 1;
            Ok(Some(Field {
                inner,
                _multipart: self,
            }))
        } else {
            Ok(None)
        }
    }
}

/// Error indicating that the multipart boundary is missing or invalid.
///
/// Carries the reason the boundary could not be read so the 4xx response is actionable.
#[skyzen::error(
    message = "Expected content type `multipart/form-data` with a boundary: {0}",
    status = StatusCode::UNSUPPORTED_MEDIA_TYPE
)]
pub struct MultipartBoundaryError(String);

/// Streams the request body, so the [`RequestBodyLimit`] in force becomes a multer
/// whole-stream constraint rather than a buffer cap; exceeding it surfaces as
/// `413 Payload Too Large` from [`Multipart::next_field`].
impl Extractor for Multipart {
    type Error = MultipartRejection;
    // Taking the body stream is synchronous — nothing is read here — so the future is ready on
    // creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            boundary_from_headers(request.headers())
                .map_err(MultipartRejection::Parse)
                .and_then(|boundary| {
                    let limit = RequestBodyLimit::of(request);
                    let limits = request.extensions().get::<MultipartLimits>().copied();
                    let body = take_body_stream::<Self>(request)?;
                    Ok(Self::from_parts(boundary, body, limit, limits))
                }),
        )
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            location: crate::openapi::ParameterLocation::Body,
            content_type: Some("multipart/form-data"),
            schema: None,
        })
    }
}

/// Represents a single multipart field.
#[derive(Debug)]
pub struct Field<'a> {
    inner: MulterField<'static>,
    _multipart: &'a mut Multipart,
}

impl Stream for Field<'_> {
    type Item = Result<Bytes, MultipartError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner)
            .poll_next(cx)
            .map(|item| item.map(|res| res.map_err(MultipartError::from_multer)))
    }
}

impl Field<'_> {
    /// Name of the form field (the `name` parameter on the `Content-Disposition` header).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.inner.name()
    }

    /// Filename from the `Content-Disposition` header when the field represents a file.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.inner.file_name()
    }

    /// Content type reported for this field, if present.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.inner.content_type().map(AsRef::as_ref)
    }

    /// Headers associated with this field.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    /// Reads the entire field contents into memory as bytes.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError`] if the payload cannot be read.
    pub async fn bytes(self) -> Result<Bytes, MultipartError> {
        self.inner
            .bytes()
            .await
            .map_err(MultipartError::from_multer)
    }

    /// Reads the entire field contents into a UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError`] if the payload cannot be read or decoded.
    pub async fn text(self) -> Result<String, MultipartError> {
        self.inner.text().await.map_err(MultipartError::from_multer)
    }

    /// Reads the next chunk from the field stream.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError`] if streaming the payload fails.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, MultipartError> {
        self.inner
            .chunk()
            .await
            .map_err(MultipartError::from_multer)
    }
}

/// Errors that can occur when processing multipart data.
#[derive(Debug)]
pub struct MultipartError {
    kind: MultipartErrorKind,
}

#[derive(Debug)]
enum MultipartErrorKind {
    Multer(multer::Error),
    TooManyFields { limit: usize },
}

impl MultipartError {
    const fn from_multer(source: multer::Error) -> Self {
        Self {
            kind: MultipartErrorKind::Multer(source),
        }
    }

    const fn too_many_fields(limit: usize) -> Self {
        Self {
            kind: MultipartErrorKind::TooManyFields { limit },
        }
    }

    /// HTTP status associated with this error.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match &self.kind {
            MultipartErrorKind::Multer(source) => match source {
                multer::Error::UnknownField { .. }
                | multer::Error::IncompleteFieldData { .. }
                | multer::Error::IncompleteHeaders
                | multer::Error::ReadHeaderFailed(..)
                | multer::Error::DecodeHeaderName { .. }
                | multer::Error::DecodeContentType(..)
                | multer::Error::NoBoundary
                | multer::Error::DecodeHeaderValue { .. }
                | multer::Error::NoMultipart
                | multer::Error::IncompleteStream => StatusCode::BAD_REQUEST,
                multer::Error::FieldSizeExceeded { .. }
                | multer::Error::StreamSizeExceeded { .. } => StatusCode::PAYLOAD_TOO_LARGE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            MultipartErrorKind::TooManyFields { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }
}

impl core::fmt::Display for MultipartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.kind {
            MultipartErrorKind::Multer(source) => {
                write!(f, "error parsing multipart request: {source}")
            }
            MultipartErrorKind::TooManyFields { limit } => {
                write!(f, "multipart request exceeded maximum of {limit} fields")
            }
        }
    }
}

impl std::error::Error for MultipartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            MultipartErrorKind::Multer(source) => Some(source),
            MultipartErrorKind::TooManyFields { .. } => None,
        }
    }
}

impl http_kit::HttpError for MultipartError {
    fn status(&self) -> StatusCode {
        self.status()
    }
}

fn boundary_from_headers(headers: &HeaderMap) -> Result<String, MultipartBoundaryError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .ok_or_else(|| MultipartBoundaryError("no content type header".to_owned()))?
        .to_str()
        .map_err(|error| MultipartBoundaryError(error.to_string()))?;
    multer::parse_boundary(content_type).map_err(|error| MultipartBoundaryError(error.to_string()))
}

pin_project! {
    struct RequestBodyStream {
        #[pin]
        body: DataStream,
    }
}

impl RequestBodyStream {
    const fn new(body: Body) -> Self {
        Self {
            body: body.into_data_stream(),
        }
    }
}

impl Stream for RequestBodyStream {
    type Item = Result<Bytes, BodyError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut body = self.project().body;
        <DataStream as LiteStream>::poll_next(body.as_mut(), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::Multipart;
    use crate::{header::HeaderValue, Body, Request};
    use http_kit::HttpError;
    use skyzen_core::Extractor;

    #[tokio::test]
    async fn parses_text_field() {
        let boundary = "boundary";
        let payload = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"greeting\"\r\n\r\nHello Skyzen!\r\n--{boundary}--\r\n"
        );

        let mut request = Request::new(Body::from_bytes(payload));
        request.headers_mut().insert(
            crate::header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
        );

        let mut multipart = Multipart::extract(&mut request).await.unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();
        assert_eq!(field.name(), Some("greeting"));
        assert_eq!(field.text().await.unwrap(), "Hello Skyzen!");
        assert!(multipart.next_field().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn missing_boundary_error() {
        let mut request = Request::new(Body::empty());
        request.headers_mut().insert(
            crate::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let error = Multipart::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), crate::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn the_body_limit_becomes_a_whole_stream_constraint() {
        let boundary = "boundary";
        let payload = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"greeting\"\r\n\r\nHello Skyzen!\r\n--{boundary}--\r\n"
        );

        let mut request = Request::new(Body::from_bytes(payload));
        request.headers_mut().insert(
            crate::header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
        );
        request
            .extensions_mut()
            .insert(skyzen_core::RequestBodyLimit::new(16));

        let mut multipart = Multipart::extract(&mut request).await.unwrap();
        let error = multipart.next_field().await.unwrap_err();
        assert_eq!(error.status(), crate::StatusCode::PAYLOAD_TOO_LARGE);
    }

    fn multipart_request(boundary: &str, fields: &[(&str, &str)]) -> Request {
        use std::fmt::Write as _;

        let mut payload = String::new();
        for (name, value) in fields {
            write!(
                payload,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .unwrap();
        }
        write!(payload, "--{boundary}--\r\n").unwrap();

        let mut request = Request::new(Body::from_bytes(payload));
        request.headers_mut().insert(
            crate::header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
        );
        request
    }

    #[tokio::test]
    async fn default_field_count_limit_rejects_excess_fields() {
        let fields: Vec<(String, &'static str)> = (0..=super::DEFAULT_MAX_FIELDS)
            .map(|i| (format!("f{i}"), "x"))
            .collect();
        let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let mut request = multipart_request("boundary", &refs);

        let mut multipart = Multipart::extract(&mut request).await.unwrap();
        for _ in 0..super::DEFAULT_MAX_FIELDS {
            let field = multipart.next_field().await.unwrap().unwrap();
            let _ = field.bytes().await.unwrap();
        }
        let error = multipart.next_field().await.unwrap_err();
        assert_eq!(error.status(), crate::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            HttpError::status(&error),
            crate::StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn multipart_limits_configures_and_lifts_field_count_and_size() {
        use super::MultipartLimits;

        let mut request = multipart_request("boundary", &[("a", "1"), ("b", "2"), ("c", "3")]);
        request
            .extensions_mut()
            .insert(MultipartLimits::new().max_fields(Some(2)));

        let mut multipart = Multipart::extract(&mut request).await.unwrap();
        assert_eq!(
            multipart
                .next_field()
                .await
                .unwrap()
                .unwrap()
                .text()
                .await
                .unwrap(),
            "1"
        );
        assert_eq!(
            multipart
                .next_field()
                .await
                .unwrap()
                .unwrap()
                .text()
                .await
                .unwrap(),
            "2"
        );
        let error = multipart.next_field().await.unwrap_err();
        assert_eq!(error.status(), crate::StatusCode::PAYLOAD_TOO_LARGE);

        // Lifting the field-count limit allows all 3 fields.
        let mut request = multipart_request("boundary", &[("a", "1"), ("b", "2"), ("c", "3")]);
        request
            .extensions_mut()
            .insert(MultipartLimits::new().without_field_count_limit());
        let mut multipart = Multipart::extract(&mut request).await.unwrap();
        for _ in 0..3 {
            assert!(multipart.next_field().await.unwrap().is_some());
        }
        assert!(multipart.next_field().await.unwrap().is_none());

        // Per-field size limit rejects an oversized field with 413 even when BodyLimit is disabled.
        let mut request = multipart_request("boundary", &[("big", "123456")]);
        request
            .extensions_mut()
            .insert(skyzen_core::RequestBodyLimit::disabled());
        request
            .extensions_mut()
            .insert(MultipartLimits::new().max_field_size(Some(5)));
        let mut multipart = Multipart::extract(&mut request).await.unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();
        let error = field.bytes().await.unwrap_err();
        assert_eq!(error.status(), crate::StatusCode::PAYLOAD_TOO_LARGE);

        // Lifting the per-field size limit allows the same 6-byte field.
        let mut request = multipart_request("boundary", &[("big", "123456")]);
        request
            .extensions_mut()
            .insert(skyzen_core::RequestBodyLimit::disabled());
        request
            .extensions_mut()
            .insert(MultipartLimits::new().without_field_size_limit());
        let mut multipart = Multipart::extract(&mut request).await.unwrap();
        let field = multipart.next_field().await.unwrap().unwrap();
        assert_eq!(field.text().await.unwrap(), "123456");
    }
}
