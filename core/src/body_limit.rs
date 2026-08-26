//! The per-request cap on how many body bytes an extractor may buffer.

use core::convert::Infallible;

use http_kit::Request;

use crate::Extractor;

/// How many request-body bytes the body extractors may buffer for one request.
///
/// The router inserts this into every request's extensions before dispatch, defaulting to
/// [`RequestBodyLimit::DEFAULT`]. A `BodyLimit` middleware attached to a route (or as a router
/// layer) replaces it for the requests it covers, and body extractors read it back with
/// [`RequestBodyLimit::of`].
///
/// The extension is the contract: any extractor that buffers a body is expected to honour it and
/// reject an oversized payload with `413 Payload Too Large`.
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
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(Self::of(request))
    }
}

#[cfg(test)]
mod tests {
    use super::RequestBodyLimit;
    use http_kit::{Body, Request};

    #[test]
    fn defaults_to_two_mebibytes() {
        let request = Request::new(Body::empty());
        assert_eq!(
            RequestBodyLimit::of(&request).max_bytes(),
            Some(RequestBodyLimit::DEFAULT)
        );
    }

    #[test]
    fn reads_back_an_inserted_override() {
        let mut request = Request::new(Body::empty());
        request
            .extensions_mut()
            .insert(RequestBodyLimit::disabled());
        assert_eq!(RequestBodyLimit::of(&request).max_bytes(), None);
    }
}
