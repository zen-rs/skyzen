//! The tunnel a proxied `101 Switching Protocols` opens: the client's connection and the upstream's,
//! joined byte for byte until both sides have closed.
//!
//! Both ends live on one thread. The server runs an [`OnUpgrade`] session on the thread that served
//! the connection, and that thread built the router, so it built the [`Proxy`](super::Proxy) and
//! its backend's pool. That is the one thread where the upstream's [`Switched`] stream can be
//! claimed, so the session claims it and copies both ways right there, owning both streams.

use std::{
    future::Future,
    io,
    pin::Pin,
    task::{ready, Context, Poll},
};

use http_kit::utils::{AsyncRead, AsyncWrite};
use skyzen_core::upgrade::{OnUpgrade, Upgraded};
use zenwave::backend::{Switched, Upgrade};

/// How much one direction reads before writing it on.
const BUFFER: usize = 16 * 1024;

/// The session that joins the client's switched connection to the upstream's.
pub(super) fn session(upgrade: Upgrade) -> OnUpgrade {
    OnUpgrade::new(move |client: Upgraded| async move {
        let upstream = match upgrade.switched().await {
            Ok(upstream) => upstream,
            Err(error) => {
                tracing::warn!("the upstream connection ended before it switched: {error}");
                return;
            }
        };
        if let Err(error) = Joined::new(client, upstream).await {
            tracing::debug!("a proxied upgrade ended with an error: {error}");
        }
    })
}

/// One direction of the tunnel: what was read from one side and not yet written to the other.
struct Direction {
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    /// The reader reached its end; what remains is to write the rest and close the writer.
    read_done: bool,
    /// Bytes were written since the writer was last flushed.
    unflushed: bool,
    done: bool,
}

impl Direction {
    fn new() -> Self {
        Self {
            buffer: vec![0; BUFFER].into_boxed_slice(),
            start: 0,
            end: 0,
            read_done: false,
            unflushed: false,
            done: false,
        }
    }

    /// Copy from `reader` to `writer` until the reader ends, then close the writer, so the end of
    /// one side's stream reaches the other as the end of its own.
    fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<()>>
    where
        R: AsyncRead + ?Sized,
        W: AsyncWrite + ?Sized,
    {
        loop {
            if self.start == self.end && !self.read_done {
                match reader.as_mut().poll_read(cx, &mut self.buffer) {
                    Poll::Ready(Ok(0)) => self.read_done = true,
                    Poll::Ready(Ok(read)) => {
                        self.start = 0;
                        self.end = read;
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => {
                        // Nothing more to send for now: what was written must not sit in a buffer.
                        if self.unflushed {
                            ready!(writer.as_mut().poll_flush(cx))?;
                            self.unflushed = false;
                        }
                        return Poll::Pending;
                    }
                }
            }
            while self.start < self.end {
                let written = ready!(writer
                    .as_mut()
                    .poll_write(cx, &self.buffer[self.start..self.end]))?;
                if written == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                self.start += written;
                self.unflushed = true;
            }
            if self.read_done {
                ready!(writer.as_mut().poll_close(cx))?;
                self.done = true;
                return Poll::Ready(Ok(()));
            }
        }
    }
}

/// Both directions of the tunnel, done once each side's stream has ended and been closed on the
/// other.
struct Joined {
    client: Upgraded,
    upstream: Switched,
    up: Direction,
    down: Direction,
}

impl Joined {
    fn new(client: Upgraded, upstream: Switched) -> Self {
        Self {
            client,
            upstream,
            up: Direction::new(),
            down: Direction::new(),
        }
    }
}

impl Future for Joined {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if !this.up.done {
            if let Poll::Ready(Err(error)) =
                this.up
                    .poll_copy(cx, Pin::new(&mut this.client), Pin::new(&mut this.upstream))
            {
                return Poll::Ready(Err(error));
            }
        }
        if !this.down.done {
            if let Poll::Ready(Err(error)) =
                this.down
                    .poll_copy(cx, Pin::new(&mut this.upstream), Pin::new(&mut this.client))
            {
                return Poll::Ready(Err(error));
            }
        }
        if this.up.done && this.down.done {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}
