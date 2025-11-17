use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll};

use crate::{
    extract::Extractor,
    header::{HeaderMap, CONTENT_TYPE},
    Body, Request, ResultExt, StatusCode,
};
use futures_core::Stream;
use http_kit::utils::{Bytes, Stream as LiteStream};
use multer::Field as MulterField;
use pin_project_lite::pin_project;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Extractor that parses `multipart/form-data` bodies.
#[derive(Debug)]
pub struct Multipart {
    inner: multer::Multipart<'static>,
}

impl Multipart {
    fn from_parts(boundary: String, body: Body) -> Self {
        Self {
            inner: multer::Multipart::new(RequestBodyStream::new(body), boundary),
        }
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

impl Extractor for Multipart {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        let boundary = boundary_from_headers(request.headers())
            .ok_or(MultipartBoundaryError)
            .status(StatusCode::UNSUPPORTED_MEDIA_TYPE)?;

        let body = mem::replace(request.body_mut(), Body::empty());
        Ok(Self::from_parts(boundary, body))
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

/// Error raised when `multipart/form-data` boundary metadata is missing or invalid.
#[derive(Debug)]
#[allow(dead_code)]
#[skyzen::error(
    status = StatusCode::UNSUPPORTED_MEDIA_TYPE,
    message = "Expected content type `multipart/form-data` with a boundary"
)]
pub struct MultipartBoundaryError;

fn boundary_from_headers(headers: &HeaderMap) -> Option<String> {
    let content_type = headers.get(CONTENT_TYPE)?.to_str().ok()?;
    multer::parse_boundary(content_type).ok()
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
    type Item = Result<Bytes, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut body = self.project().body;
        <Body as LiteStream>::poll_next(body.as_mut(), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::{Multipart, MultipartBoundaryError};
    use crate::{header::HeaderValue, Body, Request};
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
        assert!(error.downcast_ref::<MultipartBoundaryError>().is_some());
    }
}
