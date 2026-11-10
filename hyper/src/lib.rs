#![warn(missing_docs, missing_debug_implementations)]

//! Hyper server backend for the Skyzen framework.
//!
//! This crate provides [`Hyper`], which implements the [`Server`] trait from `skyzen-core`:
//! HTTP/1.1 (upgrades included) and prior-knowledge HTTP/2 over an `http-kit-io` TCP listener,
//! driving a `!Send` endpoint on the thread that calls [`Server::serve`]. It serves every
//! `http-kit-io` driver, the completion-based `compio` one included.
//!
//! [`serve_connection`] is the per-connection half, for a runtime that accepts connections itself
//! — over TLS, where ALPN picks the [`Protocol`], or a unix socket — and only needs hyper to speak
//! HTTP on them. A TLS server that also serves HTTP/3 on UDP announces it with
//! [`IntoService::with_alt_svc`].
//!
//! # Nothing here is `Send`
//!
//! hyper's `with_upgrades()` boxes the connection as a `Send` stream, which a completion-based
//! driver's socket is not. This crate never uses it: an HTTP/1 connection runs through hyper's
//! `without_shutdown`, which resolves when an upgrade is pending and hands the stream and the
//! bytes read past the request back, and the upgrade session then runs on that stream directly.
//! HTTP/2 streams are spawned on the connection's `LocalExecutor` through hyper's executor hook.
//!
//! See the [`embed_hyper.rs`](https://github.com/zen-rs/skyzen/blob/main/examples/embed_hyper.rs)
//! example for a complete setup.

use core::future::Future;
use executor_core::{LocalExecutor, Task};
use futures_util::future::{poll_fn, select, Either};
use http_kit::utils::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use http_kit_io::{IntoPoll, Io, Listener};
use hyper::server::conn::{http1::Builder as Http1Builder, http2::Builder as Http2Builder};
use skyzen_core::{
    upgrade::{UpgradeAvailable, Upgraded},
    Endpoint, Extensions, PeerAddress, Server,
};
use std::convert::Infallible;
use std::mem::MaybeUninit;
use std::pin::{pin, Pin};
use std::ptr;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tracing::{debug, error};

mod service;
pub use service::{AltSvc, IntoService, SkyzenBody};

/// The HTTP/2 connection preface (RFC 9113 §3.4).
pub const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Hyper-based [`Server`] implementation.
///
/// # Protocol support
///
/// HTTP/1.1 is served with upgrades enabled (WebSocket works out of the box). HTTP/2 is detected
/// by sniffing the client connection preface, so h2 over a bare TCP listener is *prior-knowledge
/// h2c only*. A runtime that terminates TLS itself negotiates the protocol through ALPN and hands
/// the decrypted stream to [`serve_connection`].
///
/// # Peer addresses
///
/// The listener reports each connection's remote address, which every request carries as a
/// `PeerAddr` so the `PeerAddr`/`ClientIp` extractors resolve.
///
/// # Drivers
///
/// hyper reads and writes through borrowed buffers, so it drives the stream `http-kit-io` offers
/// as [`IntoPoll`]. On the `async-io` and `tokio` drivers that costs nothing; on `compio` the
/// capability is a `CopyingPoll`, which copies every read and write through a buffer of its own,
/// so hyper on `io_uring` or IOCP pays that copy and the completion-based h1 server does not.
/// Nothing the endpoint runs has to be `Send` on any of them.
#[derive(Debug, Default, Clone, Copy)]
pub struct Hyper;

impl<I: Io> Server<I> for Hyper {
    type Listeners = I::TcpListener;

    async fn serve<Ex, E>(
        &self,
        io: I,
        executor: Ex,
        listener: I::TcpListener,
        endpoint: E,
        shutdown: impl Future<Output = ()> + 'static,
    ) where
        Ex: LocalExecutor + 'static,
        E: Endpoint + Clone + 'static,
    {
        self.serve_listener(io, executor, listener, endpoint, shutdown)
            .await;
    }
}

impl Hyper {
    /// [`Server::serve`] over any `http-kit-io` [`Listener`] — TCP, or a unix-domain socket.
    ///
    /// The semantics are `serve`'s. Requests carry a `PeerAddr` only when the listener's address
    /// is a [`PeerAddress`] with a socket address, so those on a unix socket carry none.
    pub async fn serve_listener<I, L, Ex, E>(
        &self,
        io: I,
        executor: Ex,
        listener: L,
        endpoint: E,
        shutdown: impl Future<Output = ()> + 'static,
    ) where
        I: Io,
        L: Listener,
        L::Stream: IntoPoll + 'static,
        <L::Stream as IntoPoll>::Poll: 'static,
        L::Addr: PeerAddress,
        Ex: LocalExecutor + 'static,
        E: Endpoint + Clone + 'static,
    {
        let executor = Rc::new(executor);
        // Nothing is ever sent on either channel. Dropping `stop` closes it, which every
        // connection watches to begin its graceful shutdown; each connection holds a clone of
        // `live`, so `drained` resolves once the last one has closed.
        let (stop, stopping) = async_channel::bounded::<Infallible>(1);
        let (live, drained) = async_channel::bounded::<Infallible>(1);

        let mut shutdown = pin!(shutdown);
        loop {
            let accept = pin!(listener.accept());
            let (stream, peer) = match select(accept, shutdown.as_mut()).await {
                Either::Left((Ok(accepted), _)) => accepted,
                Either::Left((Err(error), _)) => {
                    error!("accept failed: {error}");
                    continue;
                }
                Either::Right(((), _)) => break,
            };
            let peer = peer.socket_addr();
            debug!(?peer, "accepted a connection");

            let service = IntoService::new(endpoint.clone());
            let service = match peer {
                Some(peer) => service.with_peer_addr(peer),
                None => service,
            };
            let task_executor = Rc::clone(&executor);
            let stopping = stopping.clone();
            let live = live.clone();
            let io = io.clone();
            executor
                .spawn_local(async move {
                    let _live = live;
                    // The driver's own timer bounds the sniff; hyper's timers cannot be one.
                    let sniff = pin!(sniff_preface(stream.into_poll(), HTTP2_PREFACE));
                    let stream = match select(sniff, pin!(io.sleep(SNIFF_TIMEOUT))).await {
                        Either::Left((result, _)) => result,
                        Either::Right(((), _)) => Err(sniff_timed_out()),
                    };
                    let stream = match stream {
                        Ok(sniffed) => sniffed,
                        Err(error) => {
                            debug!("failed to read the connection preface: {error}");
                            return;
                        }
                    };
                    let (stream, protocol) = stream;
                    serve_connection(stream, protocol, service, task_executor, async move {
                        // Resolves with an error once `stop` is dropped.
                        let _ = stopping.recv().await;
                    })
                    .await;
                })
                .detach();
        }

        drop(stop);
        drop(live);
        let _ = drained.recv().await;
    }
}

/// Which HTTP version a connection speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/1.1, upgrades included.
    Http1,
    /// HTTP/2.
    Http2,
}

/// Serve one connection with hyper until it closes, or until `stop` resolves.
///
/// Once `stop` resolves the connection is shut down gracefully: HTTP/1 finishes the exchange in
/// flight and closes, HTTP/2 sends `GOAWAY` and drains its open streams. HTTP/1 requests carry
/// [`UpgradeAvailable`], so a handler may answer them with a `101`; HTTP/2 streams cannot be
/// upgraded and do not. Errors are logged and end only this connection.
///
/// The session of an upgrade runs as a task of its own on `executor`, which outlives this call, so
/// a connection held by a WebSocket does not hold up a graceful shutdown. Neither `stream` nor
/// anything the endpoint returns has to be `Send`.
pub async fn serve_connection<S, E, Ex>(
    stream: S,
    protocol: Protocol,
    service: IntoService<E>,
    executor: Rc<Ex>,
    stop: impl Future<Output = ()>,
) where
    S: AsyncRead + AsyncWrite + Unpin + 'static,
    E: Endpoint + Clone + 'static,
    Ex: LocalExecutor + 'static,
{
    let mut stop = pin!(stop);
    match protocol {
        Protocol::Http2 => {
            let mut builder = Http2Builder::new(LocalHyperExecutor(executor));
            // Without a timer hyper silently disables its h2 keep-alive.
            builder.timer(AsyncIoTimer);
            let mut connection = pin!(builder.serve_connection(ConnectionWrapper(stream), service));
            let result = match select(connection.as_mut(), stop).await {
                Either::Left((result, _)) => result,
                Either::Right(((), _)) => {
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = result {
                error!("hyper h2 connection error: {error}");
            }
        }
        Protocol::Http1 => {
            let mut extensions = Extensions::new();
            extensions.insert(UpgradeAvailable);
            let service = service.with_extensions(extensions);
            let mut builder = Http1Builder::new();
            // Without a timer hyper silently disables its header read timeout, so a slow-loris
            // client would hold the connection.
            builder.timer(AsyncIoTimer);
            let upgrade = service.upgrade_slot();
            let mut connection = builder.serve_connection(ConnectionWrapper(stream), service);
            // `without_shutdown` resolves, with the stream still open, when an upgrade is
            // pending as well as when the connection is done; unlike `with_upgrades` it asks
            // nothing of the stream, `Send` least of all.
            let mut stopping = false;
            let result = poll_fn(|cx| {
                if !stopping && stop.as_mut().poll(cx).is_ready() {
                    stopping = true;
                    Pin::new(&mut connection).graceful_shutdown();
                }
                connection.poll_without_shutdown(cx)
            })
            .await;
            if let Err(error) = result {
                error!("hyper h1 connection error: {error}");
                return;
            }

            let parts = connection.into_parts();
            let mut stream = parts.io.0;
            let Some(session) = upgrade.take() else {
                // Without the shutdown hyper would have made, close the stream here.
                if let Err(error) = stream.close().await {
                    debug!("closing the connection failed: {error}");
                }
                return;
            };
            let stream = Upgraded::new(Replay {
                buffered: parts.read_buf,
                inner: stream,
            });
            executor.spawn_local(session.run(stream)).detach();
        }
    }
}

/// An upgraded stream that first replays what the client sent after its upgrade request.
struct Replay<S> {
    buffered: http_kit::utils::Bytes,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Replay<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.buffered.is_empty() {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        let n = buf.len().min(this.buffered.len());
        buf[..n].copy_from_slice(&this.buffered.split_to(n));
        Poll::Ready(Ok(n))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Replay<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// Maximum time allowed for the pre-serve protocol sniff.
///
/// Without this, a client that connects and never sends a byte would park the
/// connection task forever, since hyper's own header read timeout only starts
/// once the connection is handed to it.
pub const SNIFF_TIMEOUT: Duration = Duration::from_secs(10);

/// [`hyper::rt::Timer`] backed by [`async_io::Timer`], usable on any executor.
///
/// Hyper's HTTP/1 header read timeout (30s by default) and HTTP/2 keep-alive
/// are silently disabled unless a timer is configured on the connection
/// builders, so we always install this one.
#[derive(Debug, Default, Clone, Copy)]
pub struct AsyncIoTimer;

struct AsyncIoSleep(async_io::Timer);

impl Future for AsyncIoSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(|_| ())
    }
}

impl hyper::rt::Sleep for AsyncIoSleep {}

impl hyper::rt::Timer for AsyncIoTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(AsyncIoSleep(async_io::Timer::after(duration)))
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(AsyncIoSleep(async_io::Timer::at(deadline)))
    }
}

/// Spawns hyper's HTTP/2 stream tasks on the connection's own thread.
///
/// hyper puts no `Send` bound on what it spawns here, so a stream may run a `!Send` endpoint.
struct LocalHyperExecutor<Ex>(Rc<Ex>);

impl<Ex> Clone for LocalHyperExecutor<Ex> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl<Fut, Ex> hyper::rt::Executor<Fut> for LocalHyperExecutor<Ex>
where
    Fut: Future + 'static,
    Ex: LocalExecutor + 'static,
{
    fn execute(&self, fut: Fut) {
        self.0.spawn_local(fut).detach();
    }
}

struct ConnectionWrapper<C>(C);

impl<C: Unpin + AsyncRead> hyper::rt::Read for ConnectionWrapper<C> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;

        // SAFETY: `buf.as_mut()` gives us the unfilled portion of the buffer as
        // `&mut [MaybeUninit<u8>]`. We zero-initialize every byte before casting to
        // `&mut [u8]`, so the cast never exposes uninitialized memory — the inner reader is
        // arbitrary user code, and a safe `AsyncRead` implementation may legally read from the
        // buffer it is given. After the read we call `advance(n)` for exactly the `n` bytes the
        // reader reports having written.
        let buffer = unsafe {
            let uninit = buf.as_mut();
            uninit.fill(MaybeUninit::new(0));
            &mut *(ptr::from_mut(uninit) as *mut [u8])
        };

        match Pin::new(inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(n)) => {
                // SAFETY: we just wrote `n` bytes into `buffer`, must now advance `n`
                unsafe {
                    buf.advance(n);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<C: AsyncWrite + Unpin> hyper::rt::Write for ConnectionWrapper<C> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let inner = &mut self.get_mut().0;
        Pin::new(inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;
        Pin::new(inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;
        Pin::new(inner).poll_close(cx)
    }
}

/// A stream that replays the bytes a protocol sniff already read before reading on.
#[derive(Debug)]
pub struct Prefixed<C> {
    buffer: Vec<u8>,
    pos: usize,
    inner: C,
}

impl<C> Prefixed<C> {
    /// Replay `buffer` ahead of `inner`.
    #[must_use]
    pub const fn new(inner: C, buffer: Vec<u8>) -> Self {
        Self {
            buffer,
            pos: 0,
            inner,
        }
    }

    /// The wrapped stream.
    pub const fn get_ref(&self) -> &C {
        &self.inner
    }
}

impl<C: Unpin> Unpin for Prefixed<C> {}

impl<C: AsyncRead + Unpin> AsyncRead for Prefixed<C> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        if this.pos < this.buffer.len() {
            let available = this.buffer.len() - this.pos;
            let n = available.min(buf.len());
            buf[..n].copy_from_slice(&this.buffer[this.pos..this.pos + n]);
            this.pos += n;
            if this.pos == this.buffer.len() {
                this.buffer.clear();
                this.pos = 0;
            }
            return Poll::Ready(Ok(n));
        }

        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<C: AsyncWrite + Unpin> AsyncWrite for Prefixed<C> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// Read the start of a cleartext connection to tell HTTP/2 prior knowledge from HTTP/1.1,
/// within `timeout` ([`Hyper`] uses [`SNIFF_TIMEOUT`]).
///
/// The bytes read are replayed by the returned stream, so hyper sees the connection from its first
/// byte.
///
/// # Errors
///
/// Returns the read error, or `TimedOut` when the client sends nothing within the bound.
pub async fn sniff_protocol<C>(
    stream: C,
    timeout: Duration,
) -> std::io::Result<(Prefixed<C>, Protocol)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let sniff = pin!(sniff_preface(stream, HTTP2_PREFACE));
    match select(sniff, async_io::Timer::after(timeout)).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(sniff_timed_out()),
    }
}

fn sniff_timed_out() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "timed out reading connection preface",
    )
}

async fn sniff_preface<C>(mut stream: C, preface: &[u8]) -> std::io::Result<(Prefixed<C>, Protocol)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    // Large enough for the HTTP/2 preface (24 bytes); a fixed stack buffer avoids a heap
    // allocation per read.
    let mut chunk = [0u8; 24];
    let mut buf = Vec::with_capacity(preface.len());
    while buf.len() < preface.len() {
        let remaining = (preface.len() - buf.len()).min(chunk.len());
        let n = stream.read(&mut chunk[..remaining]).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if !preface.starts_with(&buf) {
            return Ok((Prefixed::new(stream, buf), Protocol::Http1));
        }
    }
    let protocol = if buf.as_slice() == preface {
        Protocol::Http2
    } else {
        Protocol::Http1
    };
    Ok((Prefixed::new(stream, buf), protocol))
}

#[cfg(test)]
mod tests {
    use super::{sniff_preface, Prefixed, Protocol, HTTP2_PREFACE};
    use http_kit::utils::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use std::collections::VecDeque;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct ChunkedStream {
        chunks: VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }

    impl ChunkedStream {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: VecDeque::from(chunks),
                written: Vec::new(),
            }
        }
    }

    impl AsyncRead for ChunkedStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            match this.chunks.pop_front() {
                Some(mut chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    if n < chunk.len() {
                        chunk.drain(..n);
                        this.chunks.push_front(chunk);
                    }
                    Poll::Ready(Ok(n))
                }
                None => Poll::Ready(Ok(0)),
            }
        }
    }

    impl AsyncWrite for ChunkedStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn read_all<R: AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 16];
        loop {
            let n = reader.read(&mut buf).await.expect("read failed");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn detects_split_h2_preface() {
        let chunks = vec![
            HTTP2_PREFACE[..4].to_vec(),
            HTTP2_PREFACE[4..9].to_vec(),
            HTTP2_PREFACE[9..].to_vec(),
        ];
        let stream = ChunkedStream::new(chunks);

        let (_prefixed, protocol) =
            futures_lite::future::block_on(sniff_preface(stream, HTTP2_PREFACE)).unwrap();
        assert_eq!(protocol, Protocol::Http2);
    }

    #[test]
    fn preserves_bytes_on_mismatch() {
        let payload = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let chunks = vec![
            payload[..2].to_vec(),
            payload[2..8].to_vec(),
            payload[8..].to_vec(),
        ];
        let stream = ChunkedStream::new(chunks);

        futures_lite::future::block_on(async {
            let (prefixed, protocol) = sniff_preface(stream, HTTP2_PREFACE).await.unwrap();
            assert_eq!(protocol, Protocol::Http1);
            assert_eq!(read_all(prefixed).await, payload);
        });
    }

    #[test]
    fn replays_the_whole_h2_preface_and_what_follows_it() {
        let payload = [HTTP2_PREFACE, b"rest"].concat();
        let stream = ChunkedStream::new(vec![payload.clone()]);

        futures_lite::future::block_on(async {
            let (prefixed, protocol) = sniff_preface(stream, HTTP2_PREFACE).await.unwrap();
            assert_eq!(protocol, Protocol::Http2);
            assert_eq!(read_all(prefixed).await, payload);
        });
    }

    #[test]
    fn writes_pass_through_to_the_sniffed_stream() {
        let stream = ChunkedStream::new(vec![b"GET / HTTP/1.1\r\n\r\n".to_vec()]);

        futures_lite::future::block_on(async {
            let (mut prefixed, protocol) = sniff_preface(stream, HTTP2_PREFACE).await.unwrap();
            assert_eq!(protocol, Protocol::Http1);
            prefixed.write_all(b"pong").await.unwrap();
            prefixed.flush().await.unwrap();
            assert_eq!(prefixed.get_ref().written, b"pong".to_vec());
        });
    }

    #[test]
    fn prefixed_reads_its_buffer_before_the_inner_stream() {
        let prefixed = Prefixed::new(ChunkedStream::new(vec![b"tail".to_vec()]), b"head".to_vec());

        let read = futures_lite::future::block_on(read_all(prefixed));
        assert_eq!(read, b"headtail".to_vec());
    }
}
