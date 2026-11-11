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
    mem,
    pin::Pin,
    task::{Context, Poll},
};
use std::io;

use http_kit::utils::{AsyncRead, AsyncWrite, Bytes};
#[cfg(any(target_os = "linux", target_os = "android"))]
use http_kit_io::splice::SpliceDuplex;
use http_kit_io::{poll::CopyingPoll, SpliceRead, SpliceWrite, Split};

use crate::middleware::LocalBoxFuture;

/// Object-safe pairing of the two stream traits, so one box holds both.
trait Stream: AsyncRead + AsyncWrite {}

impl<T: AsyncRead + AsyncWrite> Stream for T {}

/// A connection's two halves, as a server's HTTP/1.1 driver hands them back.
struct Pair<R, W> {
    read: R,
    write: W,
}

impl<R: SpliceRead + 'static, W: SpliceWrite + 'static> Split for Pair<R, W> {
    type ReadHalf = R;
    type WriteHalf = W;

    fn split(self) -> (R, W) {
        (self.read, self.write)
    }
}

/// [`Pair`] erased for [`Upgraded`] to hold: polled as one stream, or, on Linux and Android,
/// handed to a forwarder as one end of a spliced tunnel.
trait Halves {
    /// The poll-style stream over the halves.
    fn into_poll(self: Box<Self>) -> Pin<Box<dyn Stream>>;

    /// Whether both halves can splice.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn can_splice(&self) -> bool;

    /// The halves as one end of a tunnel, `replay` being what to forward first.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn into_splice(self: Box<Self>, replay: Bytes) -> SpliceDuplex;
}

impl<R: SpliceRead + 'static, W: SpliceWrite + 'static> Halves for Pair<R, W> {
    fn into_poll(self: Box<Self>) -> Pin<Box<dyn Stream>> {
        Box::pin(CopyingPoll::new(*self))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn can_splice(&self) -> bool {
        SpliceRead::can_splice(&self.read) && SpliceWrite::can_splice(&self.write)
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn into_splice(self: Box<Self>, replay: Bytes) -> SpliceDuplex {
        SpliceDuplex::new(replay, self.read, self.write)
    }
}

/// Where an [`Upgraded`] connection is: still the driver's two halves, or a poll-style stream,
/// which buffers.
enum State {
    Halves(Box<dyn Halves>),
    Poll(Pin<Box<dyn Stream>>),
    /// Transient while the state is being replaced; seen only after a panic.
    Poisoned,
}

/// A connection after its `101` response, as a byte stream.
///
/// Whatever bytes the client sent after its upgrade request are read first. It is one type
/// whichever server switched the connection: read and write it through the `futures_io` traits,
/// or, on Linux and Android, forward it with [`into_splice`](Self::into_splice) without the bytes
/// entering user space.
pub struct Upgraded {
    /// Read off the connection with the request head, not yet handed out.
    replay: Bytes,
    io: State,
}

impl Upgraded {
    /// The connection a server's driver hands back as its two halves, `replay` being the bytes it
    /// read past the upgrade request.
    ///
    /// On Linux and Android the connection can then be spliced, when both halves can.
    pub fn from_halves(
        replay: Bytes,
        read: impl SpliceRead + 'static,
        write: impl SpliceWrite + 'static,
    ) -> Self {
        Self {
            replay,
            io: State::Halves(Box::new(Pair { read, write })),
        }
    }

    /// The connection a server hands back as a poll-style stream, `replay` being the bytes it read
    /// past the upgrade request.
    ///
    /// A stream that is only readable and writable through borrowed buffers cannot be spliced.
    pub fn from_stream(replay: Bytes, stream: impl AsyncRead + AsyncWrite + 'static) -> Self {
        Self {
            replay,
            io: State::Poll(Box::pin(stream)),
        }
    }

    /// Whether [`into_splice`](Self::into_splice) would succeed: the connection was handed over
    /// as its halves, neither has been read or written through the `futures_io` traits yet, and
    /// both can move bytes through a pipe (a plain socket, or a kernel TLS one; a TLS session in
    /// user space cannot).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[must_use]
    pub fn can_splice(&self) -> bool {
        matches!(&self.io, State::Halves(halves) if halves.can_splice())
    }

    /// This connection as one end of a tunnel, for [`SpliceDuplex::join`] to forward against
    /// another: the bytes the client sent after its upgrade request are forwarded first, then both
    /// directions go through the kernel.
    ///
    /// # Errors
    ///
    /// Gives the connection back when it cannot splice (see [`can_splice`](Self::can_splice)).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn into_splice(self) -> Result<SpliceDuplex, Self> {
        if !self.can_splice() {
            return Err(self);
        }
        let Self { replay, io } = self;
        let State::Halves(halves) = io else {
            unreachable!("can_splice holds only for halves");
        };
        Ok(halves.into_splice(replay))
    }

    /// The poll-style view of the connection, made on first use.
    fn stream(&mut self) -> Pin<&mut dyn Stream> {
        if matches!(self.io, State::Halves(_)) {
            let State::Halves(halves) = mem::replace(&mut self.io, State::Poisoned) else {
                unreachable!("matched halves above");
            };
            self.io = State::Poll(halves.into_poll());
        }
        match &mut self.io {
            State::Poll(stream) => stream.as_mut(),
            State::Halves(_) => unreachable!("converted above"),
            State::Poisoned => panic!("an upgraded connection used after a panic"),
        }
    }
}

impl fmt::Debug for Upgraded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Upgraded")
            .field("replay", &self.replay.len())
            .finish_non_exhaustive()
    }
}

impl AsyncRead for Upgraded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if !self.replay.is_empty() {
            let len = self.replay.len().min(buf.len());
            buf[..len].copy_from_slice(&self.replay.split_to(len));
            return Poll::Ready(Ok(len));
        }
        self.stream().poll_read(cx, buf)
    }
}

impl AsyncWrite for Upgraded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.stream().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream().poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream().poll_close(cx)
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
