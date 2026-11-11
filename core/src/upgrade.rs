//! Handing a connection to whatever answered its upgrade.
//!
//! A handler that answers with `101 Switching Protocols` places an [`OnUpgrade`] in the response
//! extensions. The server writes the response, takes the token back out and runs its session on the
//! connection, now a plain byte stream ([`Upgraded`]).
//!
//! The connection itself never travels through `http::Extensions`: those demand `Send + Sync`, and
//! a socket on a thread-per-core driver is neither. Only the session does, as a `Send` closure; the
//! future it returns runs on the connection's thread and need not be `Send`.

use alloc::boxed::Box;
use core::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use std::io;

use http_kit::utils::{AsyncRead, AsyncWrite};

use crate::middleware::LocalBoxFuture;

/// Object-safe pairing of the two stream traits, so one box holds both.
trait Stream: AsyncRead + AsyncWrite {}

impl<T: AsyncRead + AsyncWrite> Stream for T {}

/// A connection after its `101` response, as a byte stream.
///
/// Whatever bytes the client sent after its upgrade request are read first.
pub struct Upgraded(Pin<Box<dyn Stream>>);

impl Upgraded {
    /// Wrap the connection a server hands over.
    pub fn new(io: impl AsyncRead + AsyncWrite + 'static) -> Self {
        Self(Box::pin(io))
    }
}

impl fmt::Debug for Upgraded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upgraded").finish_non_exhaustive()
    }
}

impl AsyncRead for Upgraded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.0.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for Upgraded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.as_mut().poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.as_mut().poll_close(cx)
    }
}

/// Marks a request whose connection the server can hand over after a `101`.
///
/// A server inserts it into every request it could upgrade — an HTTP/1.1 connection, never an
/// HTTP/2 stream — so a handler that answers an upgrade where nothing would pick the connection up
/// refuses with `426` instead of sending a `101` into the void.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpgradeAvailable;

type Session = Box<dyn FnOnce(Upgraded) -> LocalBoxFuture<'static, ()> + Send>;

/// The session to run on a connection once its `101` response has been written.
///
/// `http::Extensions` requires `Clone`, so the session sits in a one-slot channel every clone
/// shares, and exactly one [`OnUpgrade::run`] receives it.
#[derive(Clone)]
pub struct OnUpgrade(async_channel::Receiver<Session>);

impl fmt::Debug for OnUpgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnUpgrade").finish_non_exhaustive()
    }
}

impl OnUpgrade {
    /// Run `session` on the connection once the server has written the `101` response.
    pub fn new<F, Fut>(session: F) -> Self
    where
        F: FnOnce(Upgraded) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let (sender, receiver) = async_channel::bounded::<Session>(1);
        let session: Session = Box::new(move |io| Box::pin(session(io)));
        sender
            .try_send(session)
            .unwrap_or_else(|_| unreachable!("a fresh one-slot channel has room"));
        Self(receiver)
    }

    /// Run the session on `io`.
    ///
    /// # Panics
    ///
    /// Panics if another clone of this token already ran the session: a connection is upgraded
    /// once, and a second run would mean two servers each believe they own it.
    pub async fn run(self, io: Upgraded) {
        let session = self
            .0
            .try_recv()
            .expect("an upgrade session runs exactly once");
        session(io).await;
    }
}
