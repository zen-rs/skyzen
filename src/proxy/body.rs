//! A body on its way through the proxy, in either direction.
//!
//! The proxy never inspects a body, so its bytes need not enter user space: each frame is asked
//! for with the [`PipeOffer`] of the writer consuming it, passed through as it came, and a source
//! whose bytes are still in a socket (an HTTP/1.1 connection with a run of raw body bytes) answers
//! with a pipe that writer splices into its own socket. An HTTP/1.1 writer, the server's writing
//! the response as much as the upstream connection's writing the request, offers exactly when its
//! socket can splice, carrying the pipe it drained last, so one pipe serves a connection's whole
//! body; a writer that cannot splice (a TLS session in user space, HTTP/2, HTTP/3) offers nothing
//! and is handed data frames. Where no source can splice, or on a platform without pipes, the
//! offer is declined and data frames pass as before.

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
        match Pin::new(body).poll_frame_offering(cx, offer) {
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
