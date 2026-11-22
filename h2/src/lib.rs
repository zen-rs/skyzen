//! HTTP/2 server backend for the Skyzen framework.
//!
//! [`H2`] implements [`Server`] on `http-kit-h2`'s sans-IO protocol state machine, over
//! `http-kit-io`'s owned-buffer streams, so the same server runs on a completion-based driver
//! (compio's `io_uring` or IOCP) and on a readiness-based one (tokio, async-io) without a
//! copying poll bridge. A connection and every concurrent stream on it stay on the thread that
//! accepted it: nothing it runs has to be `Send`.
//!
//! Extended `CONNECT` requests (`:protocol`, RFC 8441) carry a [`ConnectProtocol`] marker in
//! their extensions. When the endpoint answers one with a `2xx` status and an [`OnUpgrade`]
//! session, the server spawns that session on the thread's [`LocalExecutor`] over a full-duplex
//! stream backed by the HTTP/2 stream's request and response bodies.
//!
//! On Linux and Android an unpadded request `DATA` payload is spliced into a consumer that offers
//! a pipe, and a pipe frame in a response body is spliced into outgoing `DATA` frames.

use core::{future::Future, pin::pin};
use std::{cell::Cell, convert::Infallible, net::SocketAddr, num::NonZeroUsize, rc::Rc};

use bytes::BytesMut;
use executor_core::{LocalExecutor, Task};
use futures_util::future::{select, Either};
use http_kit::{
    header::{self, HeaderValue},
    Body, Method, Request, Response,
};
pub use http_kit_h2::{FlowControlConfig, Limits, Protocol, ServerConfig, CLIENT_PREFACE};
use http_kit_h2::{ServerConnection, Service};
use http_kit_io::{Listen, Listener, ReadOwned, SendFile, SpliceRead, SpliceWrite, Split, Timer};
use skyzen_core::{
    apply_pending_headers, error_response, log_endpoint_error,
    upgrade::{ConnectProtocol, OnUpgrade, Upgraded},
    Endpoint, PeerAddr, PeerAddress, Server,
};
use tracing::{debug, error, warn};

/// HTTP/2 [`Server`] on `http-kit-h2`.
///
/// Multiplexes concurrent streams on each connection with the flow-control windows, limits and
/// deadlines [`ServerConfig`] sets, and performs RFC 9113 §6.8 two-phase graceful `GOAWAY`
/// draining when `shutdown` resolves.
#[derive(Debug, Clone, Default)]
pub struct H2 {
    config: ServerConfig,
    max_connections: Option<NonZeroUsize>,
    alt_svc: Option<HeaderValue>,
}

impl H2 {
    /// A server with these HTTP/2 connection settings.
    #[must_use]
    pub const fn new(config: ServerConfig) -> Self {
        Self {
            config,
            max_connections: None,
            alt_svc: None,
        }
    }

    /// Advertise an alternative service in every response that does not carry an `Alt-Svc` of
    /// its own, error responses included.
    ///
    /// The value is a complete `Alt-Svc` header value (RFC 7838), such as the one
    /// [`skyzen_core::AltSvc::header_value`] renders for an HTTP/3 endpoint.
    #[must_use]
    pub fn with_alt_svc(mut self, alt_svc: HeaderValue) -> Self {
        self.alt_svc = Some(alt_svc);
        self
    }

    /// Refuse connections past `max` open at once.
    ///
    /// The limit is per [`serve`](Server::serve) call, so a thread-per-core deployment, which
    /// calls it once per thread, divides its overall limit among them. A connection accepted past
    /// the limit is closed at once, before any byte is read from it.
    #[must_use]
    pub const fn with_max_connections(mut self, max: NonZeroUsize) -> Self {
        self.max_connections = Some(max);
        self
    }

    /// The connection settings this server builds each [`ServerConnection`] with.
    #[must_use]
    pub const fn config(&self) -> &ServerConfig {
        &self.config
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

impl<I> Server<I> for H2
where
    I: Listen + Timer + Clone + 'static,
{
    type Listeners = I::Listener;

    async fn serve<Ex, E>(
        &self,
        io: I,
        executor: Ex,
        listener: I::Listener,
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

impl H2 {
    /// [`Server::serve`] over any `http-kit-io` [`Listener`] — prior-knowledge h2c on TCP or a
    /// unix-domain socket — with `timer` driving the connection deadlines.
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
        <L::Stream as Split>::ReadHalf: ReadOwned + SpliceRead,
        <L::Stream as Split>::WriteHalf: SendFile + SpliceWrite,
        Ex: LocalExecutor + 'static,
        E: Endpoint + Clone + 'static,
    {
        let (stop, stopping) = async_channel::bounded::<Infallible>(1);
        let (live, drained) = async_channel::bounded::<Infallible>(1);
        let open = Rc::new(Cell::new(0_usize));
        let executor = Rc::new(executor);

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
            debug!(?peer, "accepted an h2 connection");

            let server = self.clone();
            let timer = timer.clone();
            let conn_executor = Rc::clone(&executor);
            let endpoint = endpoint.clone();
            let stopping = stopping.clone();
            let live = live.clone();
            let slot = Slot::take(&open);
            executor
                .spawn_local(async move {
                    let _slot = slot;
                    let stop = async move {
                        let _ = stopping.recv().await;
                    };
                    server
                        .serve_connection(timer, conn_executor, stream, peer, endpoint, stop, live)
                        .await;
                })
                .detach();
        }

        drop(stop);
        drop(live);
        let _ = drained.recv().await;
    }

    /// Serve one already-accepted `stream` (such as an ALPN `h2` TLS stream) until it closes or
    /// `stop` finishes draining it.
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-connection state a protocol dispatcher hands in"
    )]
    pub async fn serve_connection<T, Ex, S, E, G>(
        &self,
        timer: T,
        executor: Ex,
        stream: S,
        peer: Option<SocketAddr>,
        endpoint: E,
        stop: impl Future<Output = ()>,
        live: G,
    ) where
        T: Timer + Clone + 'static,
        Ex: LocalExecutor + 'static,
        S: Split + 'static,
        <S as Split>::ReadHalf: ReadOwned + SpliceRead,
        <S as Split>::WriteHalf: SendFile + SpliceWrite,
        E: Endpoint + Clone + 'static,
    {
        self.serve_connection_with_read_buf(
            timer,
            executor,
            stream,
            BytesMut::new(),
            peer,
            endpoint,
            stop,
            live,
        )
        .await;
    }

    /// [`serve_connection`](Self::serve_connection) seeded with opening bytes already read from
    /// `stream` into `read_buf` (such as a sniffed 24-byte `CLIENT_PREFACE`).
    #[expect(
        clippy::too_many_arguments,
        reason = "the per-connection state a protocol dispatcher hands in"
    )]
    pub async fn serve_connection_with_read_buf<T, Ex, S, E, G>(
        &self,
        timer: T,
        executor: Ex,
        stream: S,
        read_buf: BytesMut,
        peer: Option<SocketAddr>,
        endpoint: E,
        stop: impl Future<Output = ()>,
        live: G,
    ) where
        T: Timer + Clone + 'static,
        Ex: LocalExecutor + 'static,
        S: Split + 'static,
        <S as Split>::ReadHalf: ReadOwned + SpliceRead,
        <S as Split>::WriteHalf: SendFile + SpliceWrite,
        E: Endpoint + Clone + 'static,
    {
        let (read, write) = stream.split();
        let connection =
            ServerConnection::with_read_buf(read, write, read_buf, timer, self.config.clone());
        let service = H2Service {
            endpoint,
            executor,
            peer,
            alt_svc: self.alt_svc.clone(),
        };
        let outcome = connection.serve_until(&service, stop).await;
        drop(live);
        if let Err(error) = outcome {
            debug!("h2 connection ended with an error: {error}");
        }
    }
}

/// One connection's view of the endpoint.
struct H2Service<E, Ex> {
    endpoint: E,
    executor: Ex,
    peer: Option<SocketAddr>,
    alt_svc: Option<HeaderValue>,
}

impl<E, Ex> Service for H2Service<E, Ex>
where
    E: Endpoint + Clone,
    Ex: LocalExecutor,
{
    async fn call(&self, mut request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(PeerAddr::new(peer));
        }
        if method == Method::CONNECT {
            if let Some(protocol) = request.extensions().get::<Protocol>().cloned() {
                request
                    .extensions_mut()
                    .insert(ConnectProtocol::new(protocol.as_str().to_owned()));
            }
        }

        let mut endpoint = self.endpoint.clone();
        let mut response = match endpoint.respond(&mut request).await {
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
        };

        if response.status().is_success() {
            if let Some(session) = response.extensions_mut().remove::<OnUpgrade>() {
                let incoming = core::mem::replace(request.body_mut(), Body::empty());
                let (io, outgoing) = Upgraded::from_bodies(incoming);
                *response.body_mut() = outgoing;
                self.executor.spawn_local(session.run(io)).detach();
            }
        }

        if let Some(alt_svc) = &self.alt_svc {
            response
                .headers_mut()
                .entry(header::ALT_SVC)
                .or_insert_with(|| alt_svc.clone());
        }
        response
    }
}
