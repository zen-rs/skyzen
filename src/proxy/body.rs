//! A body on its way through the proxy, in either direction.
//!
//! The proxy never inspects a body, so its bytes need not enter user space: every frame is asked
//! for with a [`PipeOffer`], and a source whose bytes are still in a socket (an HTTP/1.1
//! connection with a run of raw body bytes) answers with a pipe the far side's writer splices into
//! its own socket. The proxy's own consumers, the server writing the response and the upstream
//! connection writing the request, poll without an offer, so the offer is made here; a consumer
//! that does offer has its own offer passed on instead, pipe and all. Where no source can splice,
//! or on a platform without pipes, the offer is declined and data frames pass as before.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use http_kit::{BodyFrame, FrameSource, PipeOffer};

use super::ProxyError;
use crate::{Body, BodyError};

/// Wrap `body` to pass pipe offers through it; `refuse_trailers` fails the body at trailer fields,
/// which its reader did not accept, instead of dropping them.
///
/// A body whose length is known is already in memory, has nothing to splice and no trailers, and
/// is returned as it is.
pub(super) fn forward(body: Body, refuse_trailers: bool) -> Body {
    if body.len().is_some() {
        return body;
    }
    Body::from_source(Forwarded {
        body: Some(body),
        refuse_trailers,
    })
}

/// The source [`forward`] makes.
struct Forwarded {
    /// `None` once the body failed on trailers it may not bring.
    body: Option<Body>,
    refuse_trailers: bool,
}

impl FrameSource for Forwarded {
    fn poll_next_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        offer: Option<&mut PipeOffer>,
    ) -> Poll<Option<Result<BodyFrame, BodyError>>> {
        let this = self.get_mut();
        let Some(body) = this.body.as_mut() else {
            return Poll::Ready(None);
        };
        // No limit of the proxy's own: a pipe frame holds whatever the pipe takes.
        let mut own = PipeOffer::new(usize::MAX);
        let offer = offer.unwrap_or(&mut own);
        match Pin::new(body).poll_frame_offering(cx, Some(offer)) {
            Poll::Ready(Some(Ok(BodyFrame::Trailers(_)))) if this.refuse_trailers => {
                this.body = None;
                tracing::error!(
                    "the upstream ended its response with trailers the client did not accept"
                );
                Poll::Ready(Some(Err(BodyError::Other(Box::new(
                    ProxyError::TrailersNotAccepted,
                )))))
            }
            poll => poll,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.body
            .as_ref()
            .map_or((0, Some(0)), futures_core::Stream::size_hint)
    }
}
