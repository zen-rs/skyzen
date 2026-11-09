//! `AutoETag` — weak `ETag` generation for buffered responses.

use core::future::Future;

use http_kit::{header, Method, Request, Response, StatusCode};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error,
};
use twox_hash::XxHash3_64;

/// Default ceiling for hashing a body — 256 KiB.
const DEFAULT_MAX_BODY: u64 = 256 * 1024;

/// Emits a weak `ETag` (`W/"xxh3-of-body"`) on `GET`/`HEAD` responses that don't already
/// carry one.
///
/// Bandwidth only — the body is still built and hashed every time. For responses whose
/// validators are cheaper to compute than their bodies (a revision counter, a stored hash),
/// reach for [`Conditional`][crate::responder::Conditional] instead.
///
/// Pass-through rules: anything but `GET`/`HEAD`, non-`200` statuses, responses that already
/// carry `ETag`, and bodies that are streamed or exceed [`max_body`][Self::max_body] are left
/// untouched.
#[derive(Debug, Clone, Copy)]
pub struct AutoETag {
    max_body: u64,
}

impl Default for AutoETag {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoETag {
    /// The default limits — `GET`/`HEAD` + `200` + a buffered body of at most 256 KiB.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_body: DEFAULT_MAX_BODY,
        }
    }

    /// The largest body worth hashing for a `304`. Larger bodies pass through untagged —
    /// hashing a huge response to save re-sending it is a bad trade.
    #[must_use]
    pub const fn max_body(mut self, bytes: u64) -> Self {
        self.max_body = bytes;
        self
    }
}

impl Middleware for AutoETag {
    fn handle(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> impl Future<Output = Result<Response, Error>> + Send {
        let cacheable = request.method() == Method::GET || request.method() == Method::HEAD;
        let max_body = self.max_body;
        async move {
            let mut response = next.run(request).await?;
            if !cacheable
                || response.status() != StatusCode::OK
                || response.headers().contains_key(&header::ETAG)
            {
                return Ok(response);
            }
            // `try_clone` answers Some only for an already-buffered body — streamed or SSE
            // responses answer None and pass through.
            let Some(copy) = response.body().try_clone() else {
                return Ok(response);
            };
            let Some(len) = copy.len() else {
                return Ok(response);
            };
            if len as u64 > max_body {
                return Ok(response);
            }
            let bytes = copy.into_bytes().await.map_err(Error::new)?;
            let hash = XxHash3_64::oneshot(&bytes);
            response.headers_mut().insert(
                header::ETAG,
                header::HeaderValue::try_from(format!("W/\"{hash:016x}\""))
                    .expect("a hex ETag is a valid header value"),
            );
            Ok(response)
        }
    }
}
