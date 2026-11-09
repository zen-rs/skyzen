//! The `OpenDAL` HTTP transport, backed by `zenwave`.
//!
//! `OpenDAL` services speak HTTP through [`HttpTransport`]: a request carrying a [`Buffer`] body
//! goes in, a streaming [`HttpBody`] comes out. `zenwave` answers it, so the data-plane requests
//! the azblob service issues ride the same client — and the same connection pool — as the REST
//! calls [`BlobRest`](super::rest::BlobRest) makes beside them.

use core::future;

use futures_util::TryStreamExt as _;
use http::{header, HeaderMap, Method, Request, Response, StatusCode, Uri};
use opendal::raw::{parse_content_encoding, parse_content_length};
use opendal::{Buffer, Error, ErrorKind, HttpBody, HttpTransport, Result};
use zenwave::{Body, DefaultClient, Endpoint as _, Transport};

/// How many hops a redirected request may take before its answer is taken as-is — the bound
/// `reqwest` and `zenwave` draw alike.
const MAX_REDIRECTS: u32 = 10;

/// An [`HttpTransport`] that dispatches requests through `zenwave`.
#[derive(Debug)]
pub(super) struct ZenwaveTransport {
    /// The transport the requests go out on; the per-origin connection pool lives on it, so
    /// clients built per request still share connections.
    transport: Transport,
}

impl ZenwaveTransport {
    /// A transport over the system settings: the OS proxy rules and trust store.
    pub(super) fn system() -> Self {
        Self {
            transport: Transport::system(),
        }
    }
}

impl HttpTransport for ZenwaveTransport {
    async fn fetch(&self, req: Request<Buffer>) -> Result<Response<HttpBody>> {
        // `Uri` and `Buffer` clones are cheap: a `Uri` stores its data in `Bytes`, a `Buffer` in
        // `Bytes`/`Arc`.
        let (mut parts, mut body) = req.into_parts();
        let is_head = parts.method == Method::HEAD;

        // `HttpTransport` is documented to follow redirects, and a signed request can still be
        // honored on a hop that keeps its signed parts — a shared-key signature covers the
        // account and path, not the host. zenwave's `FollowRedirect` cannot be used: it rebuilds
        // every hop with an empty body. The loop lives here instead — a `Buffer` is in-memory,
        // so the replay is a cheap clone — and follows the previous transport's policy:
        // 301/302/303 degrade a non-GET to a bodyless GET, 307/308 keep the method and body,
        // and a cross-origin hop drops the signature it would leak.
        let mut redirects = 0;
        let response = loop {
            let mut request = Request::from_parts(parts.clone(), Body::from_stream(body.clone()));
            let mut client = DefaultClient::raw(self.transport.clone());
            let response = client.respond(&mut request).await.map_err(|error| {
                Error::new(ErrorKind::Unexpected, "send http request")
                    .with_operation("zenwave::respond")
                    .with_context("url", parts.uri.to_string())
                    .with_temporary(error.is_transient())
                    .set_source(error)
            })?;

            let status = response.status();
            if redirects >= MAX_REDIRECTS
                || !matches!(
                    status,
                    StatusCode::MOVED_PERMANENTLY
                        | StatusCode::FOUND
                        | StatusCode::SEE_OTHER
                        | StatusCode::TEMPORARY_REDIRECT
                        | StatusCode::PERMANENT_REDIRECT
                )
            {
                break response;
            }
            redirects += 1;

            let Some(next_uri) = redirect_target(&parts.uri, response.headers()) else {
                break response;
            };

            if !matches!(
                status,
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
            ) && parts.method != Method::GET
                && parts.method != Method::HEAD
            {
                parts.method = Method::GET;
                body = Buffer::default();
                // The body is gone; a streamed replay could not restate the original length
                // anyway, and a stale one would now be wrong.
                parts.headers.remove(header::CONTENT_LENGTH);
            }
            if parts.uri.scheme() != next_uri.scheme()
                || parts.uri.authority() != next_uri.authority()
            {
                parts.headers.remove(header::AUTHORIZATION);
            }
            parts.uri = next_uri;
        };

        let (mut response_parts, response_body) = response.into_parts();

        // OpenDAL's own transports leave the answered URI in the response extensions.
        response_parts.extensions.insert(parts.uri.clone());

        // The declared length is only meaningful when the body is neither absent (HEAD) nor
        // transformed by a content encoding.
        let content_length =
            if is_head || parse_content_encoding(&response_parts.headers)?.is_some() {
                None
            } else {
                parse_content_length(&response_parts.headers)?
            };

        // A failure this late is a mid-transfer transport failure — a reset or truncated read —
        // which OpenDAL retries, so it is temporary the way the previous transport classified
        // body errors.
        let answered_uri = parts.uri;
        let stream = response_body
            .try_filter(|chunk| future::ready(!chunk.is_empty()))
            .map_ok(Buffer::from)
            .map_err(move |error| {
                Error::new(ErrorKind::Unexpected, "read data from http response")
                    .with_operation("zenwave::fetch")
                    .with_context("url", answered_uri.to_string())
                    .with_temporary(true)
                    .set_source(error)
            });
        Ok(Response::from_parts(
            response_parts,
            HttpBody::new(stream, content_length),
        ))
    }
}

/// The URI `Location` names, resolved against the URI it answered.
///
/// `None` — a missing or unreadable header, or a target that does not parse as a URI — means the
/// redirect cannot be followed.
fn redirect_target(current: &Uri, headers: &HeaderMap) -> Option<Uri> {
    let location = headers.get(header::LOCATION)?.to_str().ok()?;
    url::Url::parse(&current.to_string())
        .ok()?
        .join(location)
        .ok()?
        .as_str()
        .parse()
        .ok()
}
