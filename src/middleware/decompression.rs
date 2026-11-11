//! Request body decompression middleware.
//!
//! This middleware inspects the `Content-Encoding` header and decodes the request body while it
//! is read, so extractors see the representation rather than its content coding. `gzip`,
//! `deflate`, `br` and `zstd` are supported, each compiled in by its own feature
//! (`decompress-gzip`, `decompress-deflate`, `decompress-br`, `decompress-zstd`) — all enabled
//! by default. A request naming a coding the middleware does not decode is refused with
//! `415 Unsupported Media Type`, and the response carries an `Accept-Encoding` header listing
//! the codings it does decode, per RFC 9110 §15.5.16.
//!
//! The body is decoded as a stream rather than buffered first: extractors that stream
//! ([`Body`], `Multipart`) still stream, and `RequestBodyLimit` caps the *decoded* bytes, which
//! is also what stops a compression bomb.

use std::{
    error::Error as StdError,
    fmt::{self, Display},
};

#[cfg(feature = "decompress")]
use http::header::CONTENT_LENGTH;

use http::{
    header::{ACCEPT_ENCODING, CONTENT_ENCODING},
    HeaderValue, StatusCode,
};
use http_kit::{HttpError, Request, Response};
use skyzen_core::{
    error_response,
    middleware::{Middleware, Next},
    Error,
};
use smallvec::SmallVec;

/// Middleware that decodes `Content-Encoding` request bodies.
///
/// Attach it the same way as [`CompressionMiddleware`](crate::middleware::CompressionMiddleware):
/// `.with(DecompressionMiddleware::new())` on a node, `.layer(..)` on a route or router.
#[derive(Debug, Clone)]
pub struct DecompressionMiddleware {
    /// The codings this instance accepts, in preference order for the `Accept-Encoding`
    /// advertised on a `415`.
    encodings: SmallVec<[DecompressionEncoding; 4]>,
}

impl DecompressionMiddleware {
    /// A middleware that decodes every coding this build supports.
    #[must_use]
    pub fn new() -> Self {
        Self {
            encodings: compiled_encodings(),
        }
    }

    /// Restrict the accepted codings. An empty iterator restores the defaults.
    #[must_use]
    pub fn encodings(mut self, encodings: impl IntoIterator<Item = DecompressionEncoding>) -> Self {
        self.encodings.clear();
        self.encodings.extend(encodings);
        if self.encodings.is_empty() {
            self.encodings = compiled_encodings();
        }
        self
    }

    #[cfg(feature = "decompress")]
    fn accepts(&self, encoding: DecompressionEncoding) -> bool {
        self.encodings.contains(&encoding)
    }

    /// The `415` answer for a request coding this instance does not decode.
    fn unsupported(&self, coding: &str) -> Response {
        let error = UnsupportedContentEncoding {
            coding: coding.to_owned(),
        };
        let mut response = error_response(&error);
        let advertised = self
            .encodings
            .iter()
            .map(|encoding| encoding.token())
            .collect::<Vec<_>>()
            .join(", ");
        if let Ok(value) = HeaderValue::from_str(&advertised) {
            response.headers_mut().insert(ACCEPT_ENCODING, value);
        }
        response
    }

    /// The `415` answering the first coding in `codings` this instance cannot decode.
    fn refusal(&self, codings: &[(String, Coding)]) -> Option<Response> {
        codings
            .iter()
            .find(|(_, coding)| match *coding {
                Coding::Identity => false,
                #[cfg(feature = "decompress")]
                Coding::Encoding(encoding) => !self.accepts(encoding),
                Coding::Other => true,
            })
            .map(|(token, _)| self.unsupported(token))
    }
}

/// The `Content-Encoding` tokens of `request`, in declaration order.
fn content_codings(request: &Request) -> SmallVec<[(String, Coding); 4]> {
    let mut codings = SmallVec::new();
    for value in request.headers().get_all(CONTENT_ENCODING) {
        let Ok(raw) = value.to_str() else { continue };
        for part in raw.split(',') {
            let token = part.trim();
            if !token.is_empty() {
                codings.push((token.to_owned(), parse_coding(token)));
            }
        }
    }
    codings
}

/// Rewrite `request`'s body into its decoded form.
///
/// A `Content-Encoding` list is undone in reverse declaration order, matching RFC 9110's
/// "listed in the order they were applied". `identity` entries are no-ops, and every other
/// entry has already been approved by [`DecompressionMiddleware::refusal`].
#[cfg(feature = "decompress")]
fn decode_body(request: &mut Request, codings: &[(String, Coding)]) {
    let decoders = codings
        .iter()
        .filter_map(|(_, coding)| match *coding {
            Coding::Encoding(encoding) => Some(encoding),
            Coding::Identity | Coding::Other => None,
        })
        .collect::<SmallVec<[DecompressionEncoding; 4]>>();
    if decoders.is_empty() {
        return;
    }

    let mut body = std::mem::take(request.body_mut());
    for encoding in decoders.into_iter().rev() {
        body = stream::wrap_body(body, encoding);
    }
    *request.body_mut() = body;
    // The decoded stream carries neither its coding nor a length that still means anything.
    request.headers_mut().remove(CONTENT_ENCODING);
    request.headers_mut().remove(CONTENT_LENGTH);
}

impl Default for DecompressionMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for DecompressionMiddleware {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let codings = content_codings(request);
        if let Some(response) = self.refusal(&codings) {
            return Ok(response);
        }
        #[cfg(feature = "decompress")]
        decode_body(request, &codings);
        next.run(request).await
    }
}

/// Content codings [`DecompressionMiddleware`] can decode.
///
/// Each variant exists only when its feature is compiled in: a build without
/// `decompress-zstd` cannot name `Zstd`, so the accepted set can never outrun the decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecompressionEncoding {
    /// The `gzip` coding (`x-gzip` accepted), behind `decompress-gzip`.
    #[cfg(feature = "decompress-gzip")]
    Gzip,
    /// The `deflate` coding (zlib-wrapped, matching
    /// [`CompressionMiddleware`](crate::middleware::CompressionMiddleware)'s output), behind
    /// `decompress-deflate`.
    #[cfg(feature = "decompress-deflate")]
    Deflate,
    /// The `br` (brotli) coding, behind `decompress-br`.
    #[cfg(feature = "decompress-br")]
    Brotli,
    /// The `zstd` coding, behind `decompress-zstd`.
    #[cfg(feature = "decompress-zstd")]
    Zstd,
}

impl DecompressionEncoding {
    /// The token this coding is named by on the wire.
    const fn token(self) -> &'static str {
        match self {
            #[cfg(feature = "decompress-gzip")]
            Self::Gzip => "gzip",
            #[cfg(feature = "decompress-deflate")]
            Self::Deflate => "deflate",
            #[cfg(feature = "decompress-br")]
            Self::Brotli => "br",
            #[cfg(feature = "decompress-zstd")]
            Self::Zstd => "zstd",
        }
    }
}

/// Every coding this build can decode, in wire order.
fn compiled_encodings() -> SmallVec<[DecompressionEncoding; 4]> {
    SmallVec::from_iter([
        #[cfg(feature = "decompress-gzip")]
        DecompressionEncoding::Gzip,
        #[cfg(feature = "decompress-deflate")]
        DecompressionEncoding::Deflate,
        #[cfg(feature = "decompress-br")]
        DecompressionEncoding::Brotli,
        #[cfg(feature = "decompress-zstd")]
        DecompressionEncoding::Zstd,
    ])
}

/// What one `Content-Encoding` token resolves to in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coding {
    /// `identity`: explicitly no coding.
    Identity,
    /// A coding this build has a decoder for.
    #[cfg(feature = "decompress")]
    Encoding(DecompressionEncoding),
    /// A token this build cannot decode, whether the feature was disabled or the name unknown.
    Other,
}

const fn parse_coding(token: &str) -> Coding {
    if token.eq_ignore_ascii_case("identity") {
        return Coding::Identity;
    }
    #[cfg(feature = "decompress-gzip")]
    if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
        return Coding::Encoding(DecompressionEncoding::Gzip);
    }
    #[cfg(feature = "decompress-deflate")]
    if token.eq_ignore_ascii_case("deflate") {
        return Coding::Encoding(DecompressionEncoding::Deflate);
    }
    #[cfg(feature = "decompress-br")]
    if token.eq_ignore_ascii_case("br") {
        return Coding::Encoding(DecompressionEncoding::Brotli);
    }
    #[cfg(feature = "decompress-zstd")]
    if token.eq_ignore_ascii_case("zstd") {
        return Coding::Encoding(DecompressionEncoding::Zstd);
    }
    let _ = token;
    Coding::Other
}

/// A request's `Content-Encoding` is not one this middleware instance decodes.
#[derive(Debug)]
struct UnsupportedContentEncoding {
    /// The offending token, for the client-facing message.
    coding: String,
}

impl Display for UnsupportedContentEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported content encoding `{}`", self.coding)
    }
}

impl StdError for UnsupportedContentEncoding {}

impl HttpError for UnsupportedContentEncoding {
    fn status(&self) -> StatusCode {
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    }
}

/// The streaming decoders. A build with no coding compiled in has nothing to decode with, so the
/// whole machinery is absent from it.
#[cfg(feature = "decompress")]
mod stream {
    use std::{
        collections::VecDeque,
        error::Error as StdError,
        fmt::{self, Display},
        io::{self, Read},
        mem,
        pin::Pin,
        task::{Context, Poll},
    };

    use futures_core::Stream;
    use http_kit::{utils::Bytes, Body, BodyError, DataStream};

    use super::DecompressionEncoding;

    /// Bytes written per `read` on the decoding stream.
    pub(super) const DECODE_CHUNK: usize = 8 * 1024;

    /// Wrap `body` in the decoder for `encoding`.
    ///
    /// Only called for codings [`DecompressionMiddleware::accepts`](super::DecompressionMiddleware)
    /// approved, so every arm is compiled in by construction.
    pub(super) fn wrap_body(body: Body, encoding: DecompressionEncoding) -> Body {
        match encoding {
            #[cfg(feature = "decompress-gzip")]
            DecompressionEncoding::Gzip => {
                Body::from_stream(DecodeStream::new(body, 0, |reader| {
                    Ok(flate2::read::GzDecoder::new(reader))
                }))
            }
            #[cfg(feature = "decompress-deflate")]
            DecompressionEncoding::Deflate => {
                Body::from_stream(DecodeStream::new(body, 0, |reader| {
                    Ok(flate2::read::ZlibDecoder::new(reader))
                }))
            }
            #[cfg(feature = "decompress-br")]
            DecompressionEncoding::Brotli => {
                Body::from_stream(DecodeStream::new(body, 0, |reader| {
                    Ok(brotli::Decompressor::new(reader, DECODE_CHUNK))
                }))
            }
            #[cfg(feature = "decompress-zstd")]
            DecompressionEncoding::Zstd => {
                // ruzstd parses the frame header inside `new`, so the reader must already hold
                // enough bytes: magic (4) + descriptor (1) + window descriptor (1) + dictionary
                // id (4) + frame content size (8) = 18.
                const ZSTD_HEADER_MAX: usize = 18;
                Body::from_stream(DecodeStream::new(body, ZSTD_HEADER_MAX, |reader| {
                    ruzstd::decoding::StreamingDecoder::new(reader).map_err(io::Error::other)
                }))
            }
        }
    }

    /// A synchronous `io::Read` over the asynchronous request-body stream.
    ///
    /// Decoders pull coded bytes through `read`; when the queue is drained but the stream is still
    /// open `read` reports [`BodyWouldBlock`], which [`DecodeStream`] catches on the way out to mean
    /// "poll the body for more" rather than "the body is corrupt".
    struct ChunkReader {
        /// The body's data: a decoder needs bytes, so file regions are read out here.
        body: DataStream,
        queue: VecDeque<Bytes>,
        /// Bytes queued across `queue`, so `poll_fill` can stop without summing.
        buffered: usize,
        /// The body stream is exhausted: `read` may still serve `queue`, then reports clean EOF.
        done: bool,
        /// A failure the body stream reported, surfaced once the queue has drained.
        stream_error: Option<BodyError>,
    }

    impl ChunkReader {
        const fn new(body: Body) -> Self {
            Self {
                body: body.into_data_stream(),
                queue: VecDeque::new(),
                buffered: 0,
                done: false,
                stream_error: None,
            }
        }

        /// Poll the body until at least `target` bytes are queued or the stream ends.
        fn poll_fill(&mut self, cx: &mut Context<'_>, target: usize) -> Poll<()> {
            while self.buffered < target && !self.done {
                match Pin::new(&mut self.body).poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(None) => self.done = true,
                    Poll::Ready(Some(Ok(bytes))) => {
                        self.buffered += bytes.len();
                        self.queue.push_back(bytes);
                    }
                    Poll::Ready(Some(Err(error))) => {
                        self.done = true;
                        self.stream_error = Some(error);
                    }
                }
            }
            Poll::Ready(())
        }
    }

    /// Marker the [`ChunkReader`] embeds in an `io::Error` when its queue is empty and the body is
    /// still open. It rides inside the decoder's error chain — zstd's decoder wraps source errors
    /// several levels deep, so [`wants_more_input`] walks `source()` instead of trusting `kind()`.
    #[derive(Debug)]
    struct BodyWouldBlock;

    impl Display for BodyWouldBlock {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("request body is still streaming")
        }
    }

    impl StdError for BodyWouldBlock {}

    /// A body-stream error packed for `io::Error` transport through a decoder.
    #[derive(Debug)]
    struct BodyIoError(BodyError);

    impl Display for BodyIoError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            Display::fmt(&self.0, f)
        }
    }

    impl StdError for BodyIoError {}

    /// Is `error` the decoder running out of input rather than a corrupt stream?
    ///
    /// Two signals, because decoders differ: a decoder that propagates its source's `io::Error`
    /// keeps the `WouldBlock` kind; one that wraps it (zstd nests source errors several levels
    /// deep) still carries [`BodyWouldBlock`] somewhere in the `source()` chain.
    fn wants_more_input(error: &io::Error) -> bool {
        if error.kind() == io::ErrorKind::WouldBlock {
            return true;
        }
        let mut current: &(dyn StdError + 'static) = error;
        loop {
            if current.downcast_ref::<BodyWouldBlock>().is_some() {
                return true;
            }
            match current.source() {
                Some(source) => current = source,
                None => return false,
            }
        }
    }

    impl Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.queue.pop_front() {
                Some(mut chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    self.buffered -= n;
                    if n < chunk.len() {
                        self.queue.push_front(chunk.split_off(n));
                    }
                    Ok(n)
                }
                None => {
                    if let Some(error) = self.stream_error.take() {
                        Err(io::Error::other(BodyIoError(error)))
                    } else if self.done {
                        Ok(0)
                    } else {
                        Err(io::Error::new(io::ErrorKind::WouldBlock, BodyWouldBlock))
                    }
                }
            }
        }
    }

    /// A decoder `Read`ing from a [`ChunkReader`] it owns, so [`DecodeStream`] can refill the source
    /// between calls through `get_mut`.
    trait StreamDecoder: Read {
        /// The coded-input reader inside this decoder.
        fn source_mut(&mut self) -> &mut ChunkReader;
    }

    #[cfg(feature = "decompress-gzip")]
    impl StreamDecoder for flate2::read::GzDecoder<ChunkReader> {
        fn source_mut(&mut self) -> &mut ChunkReader {
            self.get_mut()
        }
    }

    #[cfg(feature = "decompress-deflate")]
    impl StreamDecoder for flate2::read::ZlibDecoder<ChunkReader> {
        fn source_mut(&mut self) -> &mut ChunkReader {
            self.get_mut()
        }
    }

    #[cfg(feature = "decompress-br")]
    impl StreamDecoder for brotli::Decompressor<ChunkReader> {
        fn source_mut(&mut self) -> &mut ChunkReader {
            self.get_mut()
        }
    }

    #[cfg(feature = "decompress-zstd")]
    impl StreamDecoder
        for ruzstd::decoding::StreamingDecoder<ChunkReader, ruzstd::decoding::FrameDecoder>
    {
        fn source_mut(&mut self) -> &mut ChunkReader {
            self.get_mut()
        }
    }

    /// The state of one [`DecodeStream`]: construction, streaming, or finished.
    #[expect(
        clippy::large_enum_variant,
        reason = "every decoder `D` owns the `ChunkReader` it reads (`StreamDecoder::source_mut`), \
                  so `Active` is at least as large as `Init`; boxing would add an allocation per \
                  body and save nothing"
    )]
    enum DecodeState<D> {
        /// `prefill` bytes are being accumulated for a constructor that parses a header eagerly.
        Init(ChunkReader),
        /// Decoding is underway.
        Active(D),
        /// Terminal: decoder EOF, failure, or construction failure already reported.
        Done,
    }

    /// A `Stream` that drives a synchronous `io::Read` decoder off the asynchronous body.
    ///
    /// `Body::from_stream` wraps it back into a [`Body`], so a decoded request body is
    /// indistinguishable from a plain one to everything downstream.
    struct DecodeStream<D> {
        state: DecodeState<D>,
        /// Build the decoder once `reader` holds `prefill` bytes (or the body ended).
        make: fn(ChunkReader) -> io::Result<D>,
        prefill: usize,
        out: Vec<u8>,
    }

    impl<D: StreamDecoder> DecodeStream<D> {
        fn new(body: Body, prefill: usize, make: fn(ChunkReader) -> io::Result<D>) -> Self {
            Self {
                state: DecodeState::Init(ChunkReader::new(body)),
                make,
                prefill,
                out: vec![0; DECODE_CHUNK],
            }
        }

        /// Fill the reader and run the constructor; `Some` carries a build failure to surface.
        fn init(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Error>> {
            let DecodeState::Init(reader) = &mut self.state else {
                unreachable!("init outside Init state");
            };
            match reader.poll_fill(cx, self.prefill) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    let DecodeState::Init(reader) =
                        mem::replace(&mut self.state, DecodeState::Done)
                    else {
                        unreachable!("state changed inside init");
                    };
                    Poll::Ready(match (self.make)(reader) {
                        Ok(decoder) => {
                            self.state = DecodeState::Active(decoder);
                            None
                        }
                        Err(error) => Some(error),
                    })
                }
            }
        }
    }

    impl<D: StreamDecoder + Unpin> Stream for DecodeStream<D> {
        type Item = Result<Bytes, io::Error>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let this = self.as_mut().get_mut();
            loop {
                match &mut this.state {
                    DecodeState::Init(_) => match this.init(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Some(error)) => return Poll::Ready(Some(Err(error))),
                        Poll::Ready(None) => {}
                    },
                    DecodeState::Active(decoder) => match decoder.read(&mut this.out) {
                        Ok(0) => {
                            this.state = DecodeState::Done;
                            return Poll::Ready(None);
                        }
                        Ok(n) => {
                            return Poll::Ready(Some(Ok(Bytes::copy_from_slice(&this.out[..n]))));
                        }
                        Err(error) if wants_more_input(&error) => {
                            match decoder.source_mut().poll_fill(cx, 1) {
                                Poll::Pending => return Poll::Pending,
                                Poll::Ready(()) => {}
                            }
                        }
                        Err(error) => {
                            this.state = DecodeState::Done;
                            return Poll::Ready(Some(Err(error)));
                        }
                    },
                    DecodeState::Done => return Poll::Ready(None),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "decompress-br")]
    use super::stream::DECODE_CHUNK;
    use super::*;
    use crate::middleware::apply;
    use core::future::Future;
    use http::header::CONTENT_LENGTH;
    use http_kit::{Body, Endpoint};
    #[cfg(feature = "decompress-br")]
    use std::io::Read;
    use std::{convert::Infallible, io::Write, mem};
    #[cfg(feature = "decompress-gzip")]
    use {http_kit::utils::Bytes, std::io};

    const PLAIN: &str = "Hello from the compressed side!";

    /// An endpoint that echoes what an extractor would see: the request body verbatim, plus
    /// whether `Content-Encoding` and `Content-Length` reached the handler at all. A body that
    /// fails to decode surfaces as the sentinel status `555`.
    #[derive(Clone)]
    struct EchoEndpoint;

    impl Endpoint for EchoEndpoint {
        type Error = Infallible;
        fn respond(
            &mut self,
            request: &mut Request,
        ) -> impl Future<Output = Result<Response, Self::Error>> {
            let body = mem::take(request.body_mut());
            let saw_encoding = request.headers().contains_key(CONTENT_ENCODING);
            let saw_length = request.headers().contains_key(CONTENT_LENGTH);
            async move {
                let bytes = match body.into_bytes().await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        let mut response = Response::new(Body::empty());
                        *response.status_mut() = StatusCode::from_u16(555).unwrap();
                        if let Ok(value) = HeaderValue::from_str(&error.to_string()) {
                            response.headers_mut().insert("x-body-error", value);
                        }
                        return Ok(response);
                    }
                };
                let mut response = Response::new(Body::from_bytes(bytes));
                response.headers_mut().insert(
                    "x-saw-encoding",
                    HeaderValue::from_str(if saw_encoding { "1" } else { "0" }).unwrap(),
                );
                response.headers_mut().insert(
                    "x-saw-length",
                    HeaderValue::from_str(if saw_length { "1" } else { "0" }).unwrap(),
                );
                Ok(response)
            }
        }
    }

    fn encoded_request(codings: &[&str], body: Vec<u8>) -> Request {
        let mut request = Request::new(Body::from_bytes(body));
        for coding in codings {
            request
                .headers_mut()
                .append(CONTENT_ENCODING, HeaderValue::from_str(coding).unwrap());
        }
        request
    }

    fn gzipped(payload: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    async fn roundtrip(codings: &[&str], encoded: Vec<u8>) -> (StatusCode, Vec<u8>, bool, bool) {
        let middleware = DecompressionMiddleware::new();
        let mut request = encoded_request(codings, encoded);
        let declared = request.body().len().unwrap_or_default();
        request
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(declared));
        let response = apply(&middleware, &mut request, EchoEndpoint)
            .await
            .unwrap();
        let status = response.status();
        let saw_encoding = response
            .headers()
            .get("x-saw-encoding")
            .and_then(|v| v.to_str().ok())
            == Some("1");
        let saw_length = response
            .headers()
            .get("x-saw-length")
            .and_then(|v| v.to_str().ok())
            == Some("1");
        let bytes = response.into_body().into_bytes().await.unwrap();
        (status, bytes.to_vec(), saw_encoding, saw_length)
    }

    #[cfg(feature = "decompress-gzip")]
    #[tokio::test(flavor = "multi_thread")]
    async fn decodes_gzip_body() {
        let encoded = gzipped(PLAIN.as_bytes());
        let (status, bytes, saw_encoding, saw_length) = roundtrip(&["gzip"], encoded).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PLAIN.as_bytes());
        assert!(!saw_encoding, "Content-Encoding must be consumed");
        assert!(!saw_length, "Content-Length must be removed");
    }

    #[cfg(feature = "decompress-deflate")]
    #[tokio::test(flavor = "multi_thread")]
    async fn decodes_deflate_body() {
        let mut compressor =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        compressor.write_all(PLAIN.as_bytes()).unwrap();
        let encoded = compressor.finish().unwrap();

        let (status, bytes, _, _) = roundtrip(&["deflate"], encoded).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PLAIN.as_bytes());
    }

    #[cfg(feature = "decompress-br")]
    #[tokio::test(flavor = "multi_thread")]
    async fn decodes_brotli_body() {
        let mut reader = brotli::CompressorReader::new(PLAIN.as_bytes(), DECODE_CHUNK, 5, 22);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).unwrap();

        let (status, bytes, _, _) = roundtrip(&["br"], encoded).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PLAIN.as_bytes());
    }

    #[cfg(feature = "decompress-zstd")]
    #[tokio::test(flavor = "multi_thread")]
    async fn decodes_zstd_body() {
        let encoded = zstd::encode_all(PLAIN.as_bytes(), 3).unwrap();

        let (status, bytes, _, _) = roundtrip(&["zstd"], encoded).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PLAIN.as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn identity_coding_passes_through() {
        let (status, bytes, saw_encoding, _) =
            roundtrip(&["identity"], PLAIN.as_bytes().to_vec()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PLAIN.as_bytes());
        assert!(saw_encoding, "identity is a no-op, header passes");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plain_body_passes_through() {
        let middleware = DecompressionMiddleware::new();
        let mut request = Request::new(Body::from_bytes(PLAIN.as_bytes().to_vec()));
        let response = apply(&middleware, &mut request, EchoEndpoint)
            .await
            .unwrap();
        let bytes = response.into_body().into_bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), PLAIN.as_bytes());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_coding_is_refused_with_415() {
        let middleware = DecompressionMiddleware::new();
        let mut request = encoded_request(&["snappy"], vec![0; 32]);
        let response = apply(&middleware, &mut request, EchoEndpoint)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let advertised = response
            .headers()
            .get(ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        for encoding in compiled_encodings() {
            assert!(
                advertised.contains(encoding.token()),
                "Accept-Encoding should advertise {}: {advertised}",
                encoding.token()
            );
        }
        let body = response.into_body().into_string().await.unwrap();
        assert!(
            body.contains("snappy"),
            "error body should name the coding: {body}"
        );
    }

    #[cfg(all(feature = "decompress-gzip", feature = "decompress-br"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn layered_codings_decode_inside_out() {
        // `Content-Encoding: gzip, br` means br was applied last: bytes = br(gzip(plain)).
        let inner = gzipped(PLAIN.as_bytes());
        let mut reader = brotli::CompressorReader::new(inner.as_slice(), DECODE_CHUNK, 5, 22);
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).unwrap();

        let (status, bytes, _, _) = roundtrip(&["gzip", "br"], encoded).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PLAIN.as_bytes());
    }

    #[cfg(feature = "decompress-gzip")]
    #[tokio::test(flavor = "multi_thread")]
    async fn corrupt_payload_fails_the_body_read() {
        let (status, _, _, _) = roundtrip(&["gzip"], b"not a gzip stream".to_vec()).await;
        assert_eq!(status, StatusCode::from_u16(555).unwrap());
    }

    #[cfg(all(feature = "decompress-gzip", feature = "decompress-zstd"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn configured_subset_refuses_other_codings() {
        let middleware = DecompressionMiddleware::new().encodings([DecompressionEncoding::Zstd]);
        let mut request = encoded_request(&["gzip"], gzipped(PLAIN.as_bytes()));

        let response = apply(&middleware, &mut request, EchoEndpoint)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let advertised = response
            .headers()
            .get(ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(advertised, "zstd");
    }

    #[cfg(feature = "decompress-gzip")]
    #[tokio::test(flavor = "multi_thread")]
    async fn empty_encodings_list_restores_defaults() {
        let middleware = DecompressionMiddleware::new().encodings([]);
        let mut request = encoded_request(&["gzip"], gzipped(PLAIN.as_bytes()));

        let response = apply(&middleware, &mut request, EchoEndpoint)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[cfg(feature = "decompress-gzip")]
    #[tokio::test(flavor = "multi_thread")]
    async fn decodes_a_chunked_gzip_body() {
        // Feed the same gzip stream as several small chunks: the decoder must refill across
        // chunk boundaries instead of failing on the first short read.
        let encoded = gzipped(&PLAIN.as_bytes().repeat(16));
        let chunks: Vec<Result<Bytes, io::Error>> = encoded
            .chunks(7)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let mut request = encoded_request(&["gzip"], Vec::new());
        *request.body_mut() = Body::from_stream(futures_util::stream::iter(chunks));

        let response = apply(&DecompressionMiddleware::new(), &mut request, EchoEndpoint)
            .await
            .unwrap();
        let bytes = response.into_body().into_bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), PLAIN.as_bytes().repeat(16).as_slice());
    }
}
