//! Response compression middleware.
//!
//! This middleware inspects the `Accept-Encoding` header and compresses responses using
//! `gzip` or `deflate` when the client signals support for those algorithms. Compression is
//! automatically skipped for responses that are already encoded, are too small, or when the
//! negotiated encoding would not improve the payload size.

use std::{cmp::Ordering, io::Write, mem};

use flate2::{
    write::{GzEncoder, ZlibEncoder},
    Compression as FlateCompression,
};
use http::{
    header::{
        ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING, VARY,
    },
    HeaderMap, HeaderValue, Method, StatusCode,
};
use http_kit::{http_error, middleware::MiddlewareError, Body, Middleware, Request, Response};
use smallvec::{smallvec, SmallVec};

type EncodingList = SmallVec<[CompressionEncoding; 3]>;

http_error!(
    /// Compression middleware encountered an unexpected error.
    pub CompressionError,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Compression middleware failed"
);

/// Middleware that conditionally compresses outgoing responses.
#[derive(Debug, Clone, Default)]
pub struct CompressionMiddleware {
    config: CompressionConfig,
}

impl CompressionMiddleware {
    /// Creates a new middleware that negotiates `gzip` or `deflate` encoding with a 512 byte
    /// default threshold.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates the minimum response size that qualifies for compression.
    #[must_use]
    pub const fn minimum_size(mut self, minimum_size: usize) -> Self {
        self.config.minimum_size = minimum_size;
        self
    }

    /// Replaces the list of supported encodings. Defaults are restored if the iterator is empty.
    #[must_use]
    pub fn encodings(mut self, encodings: impl IntoIterator<Item = CompressionEncoding>) -> Self {
        self.config.encodings.clear();
        self.config.encodings.extend(encodings);
        if self.config.encodings.is_empty() {
            self.config.encodings = default_encodings();
        }
        self
    }

    /// Sets the compression level that will be used by the selected encoder.
    #[must_use]
    pub const fn level(mut self, level: CompressionLevel) -> Self {
        self.config.level = level;
        self
    }

    fn negotiate_encoding(&self, request: &Request) -> Option<CompressionEncoding> {
        let supported = &self.config.encodings;
        // Per RFC 9110 a specific token takes precedence over `*`; the wildcard
        // only applies to encodings the client did not list explicitly.
        let mut specific: SmallVec<[Option<(f32, usize)>; 3]> = smallvec![None; supported.len()];
        let mut wildcard: Option<(f32, usize)> = None;
        let mut position = 0usize;
        for value in &request.headers().get_all(ACCEPT_ENCODING) {
            let Ok(raw) = value.to_str() else { continue };
            parse_header_value(raw, supported, &mut position, &mut specific, &mut wildcard);
        }

        let mut best: Option<Candidate> = None;
        for (idx, encoding) in supported.iter().enumerate() {
            let Some((quality, position)) = specific[idx].or(wildcard) else {
                continue;
            };
            if quality <= 0.0 {
                continue;
            }
            consider_candidate(
                &mut best,
                Candidate {
                    encoding: *encoding,
                    quality,
                    position,
                    supported_order: idx,
                },
            );
        }
        best.map(|candidate| candidate.encoding)
    }

    fn is_response_eligible(request: &Request, response: &Response) -> bool {
        if matches!(request.method(), &Method::HEAD) {
            return false;
        }

        let status = response.status();
        let code = status.as_u16();
        if code < 200 || code == 204 || code == 205 || code == 304 {
            return false;
        }

        if response.headers().contains_key(CONTENT_ENCODING) {
            return false;
        }

        // Streaming bodies of unknown length (e.g. SSE) must not be buffered.
        if response.body().len().is_none() {
            return false;
        }

        if !is_compressible_content_type(response.headers()) {
            return false;
        }

        !matches!(response.body().is_empty(), Some(true))
    }

    async fn compress_response(
        &self,
        response: &mut Response,
        encoding: CompressionEncoding,
    ) -> Result<(), CompressionError> {
        let body = mem::take(response.body_mut());
        let original = body
            .into_bytes()
            .await
            .map_err(|_| CompressionError::new())?;

        if original.len() < self.config.minimum_size {
            set_content_length(response, original.len());
            *response.body_mut() = Body::from_bytes(original);
            return Ok(());
        }

        let compressed = encoding
            .compress(original.as_ref(), self.config.level)
            .map_err(|_| CompressionError::new())?;

        if compressed.len() >= original.len() {
            set_content_length(response, original.len());
            *response.body_mut() = Body::from_bytes(original);
            return Ok(());
        }

        set_content_length(response, compressed.len());
        *response.body_mut() = Body::from_bytes(compressed);
        response
            .headers_mut()
            .insert(CONTENT_ENCODING, encoding.header_value());
        ensure_vary_accept_encoding(response.headers_mut());
        Ok(())
    }
}

impl Middleware for CompressionMiddleware {
    type Error = CompressionError;
    async fn handle<N: http_kit::Endpoint>(
        &mut self,
        request: &mut Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        let mut response = next
            .respond(request)
            .await
            .map_err(MiddlewareError::Endpoint)?;

        if Self::is_response_eligible(request, &response) {
            if let Some(encoding) = self.negotiate_encoding(request) {
                self.compress_response(&mut response, encoding)
                    .await
                    .map_err(MiddlewareError::Middleware)?;
            }
        }

        Ok(response)
    }
}

#[derive(Debug, Clone)]
struct CompressionConfig {
    minimum_size: usize,
    encodings: EncodingList,
    level: CompressionLevel,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            minimum_size: 512,
            encodings: default_encodings(),
            level: CompressionLevel::default(),
        }
    }
}

fn default_encodings() -> EncodingList {
    smallvec![CompressionEncoding::Gzip, CompressionEncoding::Deflate]
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    encoding: CompressionEncoding,
    quality: f32,
    position: usize,
    supported_order: usize,
}

/// Record the qualities announced by one `Accept-Encoding` header value.
///
/// Specific tokens land in `specific` (keyed by supported-encoding index) and
/// `*` lands in `wildcard`; the first occurrence of a token wins. Zero
/// qualities are recorded too, so an explicit `gzip;q=0` shadows a wildcard.
fn parse_header_value(
    value: &str,
    supported: &[CompressionEncoding],
    position: &mut usize,
    specific: &mut [Option<(f32, usize)>],
    wildcard: &mut Option<(f32, usize)>,
) {
    for part in value.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (token, quality) = parse_part(trimmed);
        let current_position = *position;
        *position += 1;

        match token {
            ParsedEncoding::Specific(encoding) => {
                if let Some(idx) = supported
                    .iter()
                    .position(|candidate| *candidate == encoding)
                {
                    if specific[idx].is_none() {
                        specific[idx] = Some((quality, current_position));
                    }
                }
            }
            ParsedEncoding::Wildcard => {
                if wildcard.is_none() {
                    *wildcard = Some((quality, current_position));
                }
            }
            ParsedEncoding::Identity | ParsedEncoding::Unsupported => {}
        }
    }
}

/// Pragmatic skip list of content types that are already compressed.
fn is_compressible_content_type(headers: &HeaderMap) -> bool {
    const PRE_COMPRESSED: &[&str] = &[
        "application/zip",
        "application/gzip",
        "application/x-gzip",
        "application/x-zip-compressed",
        "application/zstd",
        "application/br",
        "application/pdf",
        "application/x-7z-compressed",
        "application/x-rar-compressed",
        "application/x-bzip2",
        "application/x-xz",
    ];

    let Some(value) = headers.get(CONTENT_TYPE) else {
        return true;
    };
    let Ok(raw) = value.to_str() else {
        return true;
    };
    let mime = raw.split(';').next().unwrap_or_default().trim();

    if mime.eq_ignore_ascii_case("text/event-stream") {
        return false;
    }

    // SVG is XML text and benefits from compression, unlike raster formats.
    if mime.eq_ignore_ascii_case("image/svg+xml") {
        return true;
    }

    let bytes = mime.as_bytes();
    let has_prefix = |prefix: &[u8]| {
        bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    if has_prefix(b"image/") || has_prefix(b"video/") || has_prefix(b"audio/") {
        return false;
    }

    !PRE_COMPRESSED
        .iter()
        .any(|candidate| mime.eq_ignore_ascii_case(candidate))
}

fn consider_candidate(best: &mut Option<Candidate>, candidate: Candidate) {
    let should_replace = match best {
        None => true,
        Some(existing) => match candidate.quality.partial_cmp(&existing.quality) {
            Some(Ordering::Greater) => true,
            Some(Ordering::Equal) => {
                if candidate.position == existing.position {
                    candidate.supported_order < existing.supported_order
                } else {
                    candidate.position < existing.position
                }
            }
            Some(Ordering::Less) | None => false,
        },
    };

    if should_replace {
        *best = Some(candidate);
    }
}

fn parse_part(part: &str) -> (ParsedEncoding, f32) {
    let mut sections = part.split(';');
    let encoding = sections.next().unwrap_or_default().trim();
    let mut quality = 1.0_f32;

    for parameter in sections {
        let parameter = parameter.trim();
        if parameter.is_empty() {
            continue;
        }

        if let Some((key, value)) = parameter.split_once('=') {
            if key.trim().eq_ignore_ascii_case("q") {
                if let Some(parsed) = parse_quality(value) {
                    quality = parsed;
                } else {
                    quality = 0.0;
                }
                break;
            }
        }
    }

    (ParsedEncoding::from_token(encoding), quality)
}

fn parse_quality(raw: &str) -> Option<f32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let value = trimmed.parse::<f32>().ok()?;
    if !(0.0..=1.0).contains(&value) {
        return None;
    }

    Some(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedEncoding {
    Specific(CompressionEncoding),
    Wildcard,
    Identity,
    Unsupported,
}

impl ParsedEncoding {
    fn from_token(token: &str) -> Self {
        let token = token.trim();
        if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
            Self::Specific(CompressionEncoding::Gzip)
        } else if token.eq_ignore_ascii_case("deflate") {
            Self::Specific(CompressionEncoding::Deflate)
        } else if token == "*" {
            Self::Wildcard
        } else if token.eq_ignore_ascii_case("identity") {
            Self::Identity
        } else {
            Self::Unsupported
        }
    }
}

fn set_content_length(response: &mut Response, len: usize) {
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(len));
    response.headers_mut().remove(TRANSFER_ENCODING);
}

fn ensure_vary_accept_encoding(headers: &mut HeaderMap) {
    match headers.get_mut(VARY) {
        Some(value) => {
            if let Ok(existing) = value.to_str() {
                if existing
                    .split(',')
                    .any(|segment| segment.trim().eq_ignore_ascii_case("accept-encoding"))
                {
                    return;
                }

                let mut combined = existing.trim().to_owned();
                if !combined.is_empty() {
                    combined.push_str(", ");
                }
                combined.push_str("Accept-Encoding");
                if let Ok(updated) = HeaderValue::from_str(&combined) {
                    *value = updated;
                }
                return;
            }

            *value = HeaderValue::from_static("Accept-Encoding");
        }
        None => {
            headers.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
        }
    }
}

/// Compression algorithms supported by [`CompressionMiddleware`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionEncoding {
    /// Gzip encoding.
    Gzip,
    /// Deflate (zlib) encoding.
    Deflate,
}

impl CompressionEncoding {
    const fn header_value(self) -> HeaderValue {
        match self {
            Self::Gzip => HeaderValue::from_static("gzip"),
            Self::Deflate => HeaderValue::from_static("deflate"),
        }
    }

    fn compress(self, body: &[u8], level: CompressionLevel) -> std::io::Result<Vec<u8>> {
        let compression = level.into_impl();
        match self {
            Self::Gzip => {
                let mut encoder = GzEncoder::new(Vec::new(), compression);
                encoder.write_all(body)?;
                encoder.finish()
            }
            Self::Deflate => {
                let mut encoder = ZlibEncoder::new(Vec::new(), compression);
                encoder.write_all(body)?;
                encoder.finish()
            }
        }
    }
}

/// Compression strength used by [`CompressionMiddleware`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompressionLevel {
    /// Fast compression optimized for latency.
    Fast,
    /// Best compression ratio with a possible CPU trade-off.
    Best,
    /// Custom compression level (0-9).
    Precise(u32),
    /// Uses the zlib default.
    #[default]
    Default,
}

impl CompressionLevel {
    fn into_impl(self) -> FlateCompression {
        match self {
            Self::Fast => FlateCompression::fast(),
            Self::Best => FlateCompression::best(),
            Self::Default => FlateCompression::default(),
            Self::Precise(level) => FlateCompression::new(level.min(9)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::{GzDecoder, ZlibDecoder};
    use http::{header::CONTENT_ENCODING, HeaderValue};
    use http_kit::Endpoint;
    use std::{convert::Infallible, io::Read};

    struct StaticEndpoint {
        payload: String,
        vary: Option<HeaderValue>,
    }

    impl StaticEndpoint {
        fn new(payload: &str) -> Self {
            Self {
                payload: payload.to_owned(),
                vary: None,
            }
        }

        fn with_vary(mut self, value: HeaderValue) -> Self {
            self.vary = Some(value);
            self
        }

        fn response_body(&self) -> Body {
            Body::from_bytes(self.payload.clone())
        }

        fn payload(&self) -> &str {
            &self.payload
        }
    }

    impl Endpoint for StaticEndpoint {
        type Error = Infallible;
        async fn respond(&mut self, _request: &mut Request) -> Result<Response, Self::Error> {
            let mut response = Response::new(self.response_body());
            if let Some(value) = self.vary.clone() {
                response.headers_mut().insert(VARY, value);
            }
            Ok(response)
        }
    }

    fn request_with_encoding(value: Option<&str>) -> Request {
        let mut request = Request::new(Body::empty());
        if let Some(value) = value {
            request
                .headers_mut()
                .insert(ACCEPT_ENCODING, HeaderValue::from_str(value).unwrap());
        }
        request
    }

    async fn decode_gzip(body: Body) -> String {
        let bytes = body.into_bytes().await.unwrap();
        let mut decoder = GzDecoder::new(bytes.as_ref());
        let mut output = String::new();
        decoder.read_to_string(&mut output).unwrap();
        output
    }

    async fn decode_deflate(body: Body) -> String {
        let bytes = body.into_bytes().await.unwrap();
        let mut decoder = ZlibDecoder::new(bytes.as_ref());
        let mut output = String::new();
        decoder.read_to_string(&mut output).unwrap();
        output
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compresses_with_gzip_when_client_accepts() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        let mut request = request_with_encoding(Some("gzip"));
        let mut endpoint = StaticEndpoint::new(&"Hello World!".repeat(50));

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();
        let headers = response.headers().clone();
        let decoded = decode_gzip(response.into_body()).await;

        assert_eq!(decoded, endpoint.payload());
        assert_eq!(
            headers
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skips_compression_without_matching_encoding() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        let mut request = request_with_encoding(None);
        let mut endpoint = StaticEndpoint::new("plain body");

        let mut response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();

        assert!(response.headers().get(CONTENT_ENCODING).is_none());
        let body = response
            .body_mut()
            .take()
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        assert_eq!(body.as_ref(), endpoint.payload().as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn honors_deflate_quality_order() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        let mut request = request_with_encoding(Some("gzip;q=0.5, deflate;q=1"));
        let mut endpoint = StaticEndpoint::new(&"payload".repeat(80));

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();
        let headers = response.headers().clone();
        let decoded = decode_deflate(response.into_body()).await;

        assert_eq!(decoded, endpoint.payload());
        assert_eq!(
            headers
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("deflate")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn enforces_minimum_size() {
        let payload = "tiny";
        let mut middleware = CompressionMiddleware::new().minimum_size(payload.len() + 1);
        let mut request = request_with_encoding(Some("gzip"));
        let mut endpoint = StaticEndpoint::new(payload);

        let mut response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();
        assert!(response.headers().get(CONTENT_ENCODING).is_none());

        let body = response
            .body_mut()
            .take()
            .unwrap()
            .into_bytes()
            .await
            .unwrap();
        assert_eq!(body.as_ref(), payload.as_bytes());
    }

    struct FnEndpoint<F>(F);

    impl<F> Endpoint for FnEndpoint<F>
    where
        F: FnMut() -> Response + Send + Sync,
    {
        type Error = Infallible;
        async fn respond(&mut self, _request: &mut Request) -> Result<Response, Self::Error> {
            Ok((self.0)())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skips_streaming_bodies_of_unknown_length() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        let mut request = request_with_encoding(Some("gzip"));
        let mut endpoint = FnEndpoint(|| {
            let stream = futures_util::stream::iter(vec![Ok::<_, Infallible>("streamed chunk")]);
            Response::new(Body::from_stream(stream))
        });

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();

        assert!(response.headers().get(CONTENT_ENCODING).is_none());
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "streamed chunk");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skips_already_compressed_content_types() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        let mut request = request_with_encoding(Some("gzip"));
        let payload = "fake png bytes".repeat(50);
        let body = payload.clone();
        let mut endpoint = FnEndpoint(move || {
            let mut response = Response::new(Body::from_bytes(body.clone()));
            response
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
            response
        });

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();

        assert!(response.headers().get(CONTENT_ENCODING).is_none());
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, payload);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skips_responses_that_are_already_encoded() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        let mut request = request_with_encoding(Some("gzip"));
        let payload = "already encoded".repeat(50);
        let body = payload.clone();
        let mut endpoint = FnEndpoint(move || {
            let mut response = Response::new(Body::from_bytes(body.clone()));
            response
                .headers_mut()
                .insert(CONTENT_ENCODING, HeaderValue::from_static("br"));
            response
        });

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();

        assert_eq!(
            response
                .headers()
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("br")
        );
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, payload);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wildcard_does_not_override_explicit_zero_quality() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        // `gzip;q=0, *` forbids gzip; the wildcard covers deflate only.
        let mut request = request_with_encoding(Some("gzip;q=0, *"));
        let mut endpoint = StaticEndpoint::new(&"payload".repeat(80));

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();
        let headers = response.headers().clone();
        let decoded = decode_deflate(response.into_body()).await;

        assert_eq!(decoded, endpoint.payload());
        assert_eq!(
            headers
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("deflate")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn specific_quality_beats_wildcard_quality() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        // `*` at q=1 applies to deflate; gzip keeps its explicit q=0.5.
        let mut request = request_with_encoding(Some("*, gzip;q=0.5"));
        let mut endpoint = StaticEndpoint::new(&"payload".repeat(80));

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();
        let headers = response.headers().clone();
        let decoded = decode_deflate(response.into_body()).await;

        assert_eq!(decoded, endpoint.payload());
        assert_eq!(
            headers
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("deflate")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn appends_vary_header_once() {
        let mut middleware = CompressionMiddleware::new().minimum_size(0);
        let mut request = request_with_encoding(Some("gzip"));
        let mut endpoint = StaticEndpoint::new(&"payload".repeat(60))
            .with_vary(HeaderValue::from_static("Accept-Language"));

        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();
        let vary = response.headers().get(VARY).unwrap().to_str().unwrap();
        assert_eq!(vary, "Accept-Language, Accept-Encoding");

        // Run again to ensure we do not duplicate the header.
        let response = middleware
            .handle(&mut request, &mut endpoint)
            .await
            .unwrap();
        let vary = response.headers().get(VARY).unwrap().to_str().unwrap();
        assert_eq!(vary, "Accept-Language, Accept-Encoding");
    }
}
