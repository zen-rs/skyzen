//! Multipart form data utilities module.
//! It provides an extractor for `multipart/form-data` requests.

use core::future::{ready, Future};
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::{
    extract::Extractor,
    header::{HeaderMap, CONTENT_TYPE},
    Body, Request, StatusCode,
};
use futures_core::Stream;
use http_kit::utils::{Bytes, Stream as LiteStream};
use http_kit::BodyError;
use multer::Field as MulterField;
use pin_project_lite::pin_project;
use skyzen_core::{take_body_stream, BodyExtractorError, RequestBodyLimit};

/// Extractor that parses `multipart/form-data` bodies.
#[derive(Debug)]
pub struct Multipart {
    inner: multer::Multipart<'static>,
}

/// What a [`Multipart`] extraction can fail with: the body was unavailable, or it carried no
/// usable boundary.
pub type MultipartRejection = BodyExtractorError<MultipartBoundaryError>;

impl Multipart {
    fn from_parts(boundary: String, body: Body, limit: RequestBodyLimit) -> Self {
        let stream = RequestBodyStream::new(body);
        let inner = match limit.max_bytes() {
            // The whole multipart stream — every field plus the framing — shares the request's
            // budget, so multer stops reading at the same point a buffering extractor would.
            Some(max_bytes) => multer::Multipart::with_constraints(
                stream,
                boundary,
                multer::Constraints::new()
                    .size_limit(multer::SizeLimit::new().whole_stream(max_bytes as u64)),
            ),
            None => multer::Multipart::new(stream, boundary),
        };
        Self { inner }
    }

    /// Yields the next [`Field`] if available.
    ///
    /// # Errors
    ///
    /// Returns [`MultipartError`] if parsing the field fails.
    pub async fn next_field(&mut self) -> Result<Option<Field<'_>>, MultipartError> {
        let field = self
            .inner
            .next_field()
            .await
            .map_err(MultipartError::from_multer)?;

        Ok(field.map(|inner| Field {
            inner,
            _multipart: self,
        }))
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
                    let body = take_body_stream::<Self>(request)?;
                    Ok(Self::from_parts(boundary, body, limit))
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
    source: multer::Error,
}

impl MultipartError {
    const fn from_multer(source: multer::Error) -> Self {
        Self { source }
    }

    /// HTTP status associated with this error.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match &self.source {
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
            multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl core::fmt::Display for MultipartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "error parsing multipart request: {}", self.source)
    }
}

impl std::error::Error for MultipartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
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
        body: Body,
    }
}

impl RequestBodyStream {
    const fn new(body: Body) -> Self {
        Self { body }
    }
}

impl Stream for RequestBodyStream {
    type Item = Result<Bytes, BodyError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut body = self.project().body;
        <Body as LiteStream>::poll_next(body.as_mut(), cx)
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
}
