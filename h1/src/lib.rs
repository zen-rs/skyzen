//! HTTP/1.1 server backend for the Skyzen framework.
//!
//! [`H1`] implements [`Server`] on `http-kit-h1`'s sans-IO state machine, over `http-kit-io`'s
//! owned-buffer streams, so the same server runs on a completion-based driver (compio's `io_uring`
//! or IOCP) and on a readiness-based one (tokio, async-io) without a copy per read or write. A
//! connection lives on the thread that accepted it: nothing it runs has to be `Send`.
//!
//! An upgraded connection — a `101` the endpoint answered with — is handed back by
//! `http-kit-h1` rather than through request extensions, because a socket on a thread-per-core
//! driver is not `Send`. The endpoint's response carries an
//! [`OnUpgrade`](skyzen_core::upgrade::OnUpgrade) token instead, and the server runs that
//! session on the connection once the `101` is written.

use core::{
    future::Future,
    pin::{pin, Pin},
    task::{Context, Poll},
};
use std::{cell::Cell, convert::Infallible, io, net::SocketAddr, num::NonZeroUsize, rc::Rc};

use executor_core::{LocalExecutor, Task};
use futures_util::future::{select, Either};
use http_kit::{
    utils::{AsyncRead, AsyncWrite, Bytes},
    Request, Response,
};
use http_kit_h1::{ServerConfig, ServerConnection, ServerOutcome, Service};
use http_kit_io::{CopyingPoll, Io, Listener, Read, SendFile, Split, Timer, Write};
use skyzen_core::{
    apply_pending_headers, error_response, log_endpoint_error,
    upgrade::{OnUpgrade, UpgradeAvailable, Upgraded},
    Endpoint, PeerAddr, PeerAddress, Server,
};
use tracing::{debug, error, warn};

/// HTTP/1.1 [`Server`] on `http-kit-h1`.
///
/// Requests on a connection are answered strictly in order, keep-alive and pipelining included,
/// with the timeouts and limits [`ServerConfig`] sets. Every request carries the connection's
/// `PeerAddr` and [`UpgradeAvailable`].
#[derive(Debug, Clone, Default)]
pub struct H1 {
    config: ServerConfig,
    max_connections: Option<NonZeroUsize>,
}

impl H1 {
    /// A server with these connection settings.
    #[must_use]
    pub const fn new(config: ServerConfig) -> Self {
        Self {
            config,
            max_connections: None,
        }
    }

    /// Refuse connections past `max` open at once.
    ///
    /// The limit is per [`serve`](Server::serve) call, so a thread-per-core deployment, which
    /// calls it once per thread, divides its overall limit among them. A connection accepted past
    /// the limit is closed at once, before any byte is read from it, so a flood cannot grow the
    /// number of tasks without bound. Without a limit every connection is served.
    #[must_use]
    pub const fn with_max_connections(mut self, max: NonZeroUsize) -> Self {
        self.max_connections = Some(max);
        self
    }
}

/// Counts a connection against the limit for as long as it lives.
struct Slot(Rc<Cell<usize>>);

impl Slot {
    fn take(open: &Rc<Cell<usize>>) -> Self {
        open.set(open.get() + 1);
        Self(Rc::clone(open))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl<I> Server<I> for H1
where
    I: Io,
    <I::TcpStream as Split>::WriteHalf: SendFile,
{
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

impl H1 {
    /// [`Server::serve`] over any `http-kit-io` [`Listener`] — TCP, or a unix-domain socket — with
    /// `timer` driving the connection deadlines.
    ///
    /// The semantics are `serve`'s: it resolves once `shutdown` has, accepting has stopped and
    /// the last connection has closed. Requests carry a `PeerAddr` only when the listener's
    /// address is a [`PeerAddress`] with a socket address.
    pub async fn serve_listener<T, L, Ex, E>(
        &self,
        timer: T,
        executor: Ex,
        listener: L,
        endpoint: E,
        shutdown: impl Future<Output = ()> + 'static,
    ) where
        T: Timer + Clone + 'static,
        L: Listener,
        L::Stream: Split + 'static,
        L::Addr: PeerAddress,
        <L::Stream as Split>::WriteHalf: SendFile,
        Ex: LocalExecutor + 'static,
        E: Endpoint + Clone + 'static,
    {
        // Nothing is ever sent on either channel. Dropping `stop` closes it, which every
        // connection watches to stop reading new requests; each connection holds a clone of
        // `live`, so `drained` resolves once the last one has closed.
        let (stop, stopping) = async_channel::bounded::<Infallible>(1);
        let (live, drained) = async_channel::bounded::<Infallible>(1);
        let open = Rc::new(Cell::new(0_usize));

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
            if self
                .max_connections
                .is_some_and(|max| open.get() >= max.get())
            {
                warn!("connection limit reached; closing a connection");
                continue;
            }
            let peer = peer.socket_addr();
            debug!(?peer, "accepted a connection");

            let server = self.clone();
            let timer = timer.clone();
            let endpoint = endpoint.clone();
            let stopping = stopping.clone();
            let live = live.clone();
            let slot = Slot::take(&open);
            executor
                .spawn_local(async move {
                    let _slot = slot;
                    let stop = async move {
                        // Resolves with an error once `stop` is dropped.
                        let _ = stopping.recv().await;
                    };
                    server
                        .serve_connection(timer, stream, peer, endpoint, stop, live)
                        .await;
                })
                .detach();
        }

        drop(stop);
        drop(live);
        let _ = drained.recv().await;
    }

    /// Serve one already-accepted `stream` until it closes or `stop` resolves.
    ///
    /// This is what [`serve_listener`](Self::serve_listener) runs for each connection, public so
    /// a caller that has work between accepting and serving — a TLS handshake — can do it on the
    /// connection's own task instead of in the accept loop. `peer` is attached to every request
    /// as its `PeerAddr`. `live` is dropped once the HTTP exchange is over, before an upgraded
    /// session starts, which is how a drain stops waiting for the connection. The connection
    /// limit is the caller's.
    pub async fn serve_connection<T, S, E, G>(
        &self,
        timer: T,
        stream: S,
        peer: Option<SocketAddr>,
        endpoint: E,
        stop: impl Future<Output = ()>,
        live: G,
    ) where
        T: Timer + Clone + 'static,
        S: Split + 'static,
        <S as Split>::WriteHalf: SendFile,
        E: Endpoint + Clone + 'static,
    {
        let (read, write) = stream.split();
        let connection = ServerConnection::new(read, write, timer, self.config.clone());
        let service = H1Service { endpoint, peer };
        let outcome = connection.serve_until(&service, stop).await;
        // An upgraded connection is no longer an HTTP exchange the drain waits for.
        drop(live);
        match outcome {
            Ok(ServerOutcome::Closed) => {}
            Ok(ServerOutcome::Upgraded {
                upgraded,
                mut response,
            }) => match response.extensions.remove::<OnUpgrade>() {
                Some(session) => {
                    let stream = Replay {
                        buffered: upgraded.read_buf,
                        inner: CopyingPoll::new(Halves {
                            read: upgraded.read,
                            write: upgraded.write,
                        }),
                    };
                    session.run(Upgraded::new(stream)).await;
                }
                None => {
                    error!(
                        "the endpoint answered 101 without an upgrade session; \
                         closing the connection"
                    );
                }
            },
            Err(error) => debug!("h1 connection ended with an error: {error}"),
        }
    }
}

/// One connection's view of the endpoint.
struct H1Service<E> {
    endpoint: E,
    peer: Option<SocketAddr>,
}

impl<E: Endpoint + Clone> Service for H1Service<E> {
    async fn call(&self, mut request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(PeerAddr::new(peer));
        }
        request.extensions_mut().insert(UpgradeAvailable);

        let mut endpoint = self.endpoint.clone();
        match endpoint.respond(&mut request).await {
            Ok(response) => {
                tracing::info!(
                    method = method.as_str(),
                    path = path.as_str(),
                    status = response.status().as_u16(),
                    "request completed"
                );
                response
            }
            Err(error) => {
                log_endpoint_error(&error, &method, path.as_str());
                let mut response = error_response(&error);
                apply_pending_headers(&request, &mut response);
                response
            }
        }
    }
}

/// The two halves `http-kit-h1` hands back, put together again for [`CopyingPoll`].
struct Halves<R, W> {
    read: R,
    write: W,
}

impl<R: Read + 'static, W: Write + 'static> Split for Halves<R, W> {
    type ReadHalf = R;
    type WriteHalf = W;

    fn split(self) -> (R, W) {
        (self.read, self.write)
    }
}

/// An upgraded stream that first replays what the client sent after its upgrade request.
struct Replay<S> {
    buffered: Bytes,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Replay<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
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
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}
