//! The `OpenDAL` HTTP transport, backed by `zenwave`.
//!
//! `OpenDAL` services speak HTTP through [`HttpTransport`]: a request carrying a [`Buffer`] body
//! goes in, a streaming [`HttpBody`] comes out. `zenwave` answers it, so the data-plane requests
//! the azblob service issues ride the same client — and the same connection pool — as the REST
//! calls [`BlobRest`](super::rest::BlobRest) makes beside them.

use core::future;

use futures_util::TryStreamExt as _;
use http::{Request, Response};
use opendal::raw::{parse_content_encoding, parse_content_length};
use opendal::{Buffer, Error, ErrorKind, HttpBody, HttpTransport, Result};
use zenwave::{Body, Endpoint as _, Transport};

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
        // `Uri` stores its data in `Bytes`, which makes this clone cheap.
        let uri = req.uri().clone();
        let is_head = req.method() == http::Method::HEAD;

        let (parts, body) = req.into_parts();
        let mut request = Request::from_parts(parts, Body::from_stream(body));

        // Redirects are what `client_with` adds over the raw backend, and `HttpTransport` is
        // documented to follow them.
        let mut client = zenwave::client_with(self.transport.clone());
        let response = client.respond(&mut request).await.map_err(|error| {
            Error::new(ErrorKind::Unexpected, "send http request")
                .with_operation("zenwave::respond")
                .with_context("url", uri.to_string())
                .with_temporary(error.is_transient())
                .set_source(error)
        })?;

        let (mut parts, body) = response.into_parts();

        // OpenDAL's own transports leave the answered URI in the response extensions.
        parts.extensions.insert(uri.clone());

        // The declared length is only meaningful when the body is neither absent (HEAD) nor
        // transformed by a content encoding.
        let content_length = if is_head || parse_content_encoding(&parts.headers)?.is_some() {
            None
        } else {
            parse_content_length(&parts.headers)?
        };

        // A failure this late is a mid-transfer transport failure — a reset or truncated read —
        // which OpenDAL retries, so it is temporary the way the previous transport classified
        // body errors.
        let stream = body
            .try_filter(|chunk| future::ready(!chunk.is_empty()))
            .map_ok(Buffer::from)
            .map_err(move |error| {
                Error::new(ErrorKind::Unexpected, "read data from http response")
                    .with_operation("zenwave::fetch")
                    .with_context("url", uri.to_string())
                    .with_temporary(true)
                    .set_source(error)
            });
        Ok(Response::from_parts(
            parts,
            HttpBody::new(stream, content_length),
        ))
    }
}
