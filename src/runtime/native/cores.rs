//! Thread-per-core serving.
//!
//! One thread per core, each running a compio runtime of its own. Every thread has its own
//! listener on the same address, builds its own router from the application's
//! [`EndpointFactory`], and serves the connections it accepts to completion without ever handing
//! one to another thread: nothing a request touches is shared except the services the factory
//! captured, which are `Send + Sync`.
//!
//! On Linux and Android each thread binds the address itself with `SO_REUSEPORT` and the kernel
//! spreads incoming connections over the listeners. Elsewhere that option does not balance (macOS
//! and the BSDs hand every connection to one listener; Windows has no equivalent), so the threads
//! share one socket through duplicated descriptors and whichever is idle takes the next
//! connection: the same program, balanced by the accept queue instead of by the kernel's hash.
//!
//! Windows cannot share a socket that way, since a completion port takes a socket only once: one
//! thread accepts and deals the connections out to the serving threads (see [`super::handoff`]).

use core::{convert::Infallible, future::Future};
use std::{io, num::NonZeroUsize, rc::Rc, sync::Arc, thread};

use async_channel::{bounded, Receiver, Sender};
use executor_core::{compio::Runtime, AnyExecutor};
use http_kit_io::{compio::Compio, IntoPoll, Io, Listener, SendFile, Split, TcpOptions};
use skyzen_core::{EndpointFactory, PeerAddress};
use skyzen_h1::H1;
use skyzen_hyper::{AltSvc, Hyper};
use tracing::error;

use super::{
    handoff::{Acceptor, Incoming},
    http3::Http3,
    tls::{Protocols, ServerTls},
    HttpServer, RuntimeFailure,
};
use crate::{
    runtime::{context::ShutdownGuard, WorkerContext},
    Endpoint, Request, Response,
};

/// A bound, listening socket waiting for the thread that will serve it.
#[derive(Debug)]
pub(super) enum Socket {
    /// A TCP listener of this thread's own. Windows never has one: its threads are dealt their
    /// connections.
    #[cfg(not(windows))]
    Tcp(std::net::TcpListener),
    /// The connections a separate acceptor deals to this thread, where one socket cannot be
    /// shared between threads.
    #[cfg_attr(
        not(any(windows, test)),
        expect(dead_code, reason = "only Windows selects the handoff path")
    )]
    Handoff(Incoming),
    /// A filesystem unix socket.
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
}

/// The sockets of every serving thread, and the acceptor that feeds them when they share none.
#[derive(Debug)]
pub(super) struct Bound {
    /// One per serving thread.
    pub(super) sockets: Vec<Socket>,
    /// Deals connections to the `Handoff` sockets; `None` when the threads accept for themselves.
    pub(super) acceptor: Option<Acceptor>,
    /// The UDP socket each thread serves HTTP/3 on, in the order of `sockets`; `None` for a
    /// thread that does not run QUIC. Empty when HTTP/3 is not served.
    pub(super) udp: Vec<Option<std::net::UdpSocket>>,
}

impl Socket {
    /// The address a TCP socket listens on, or `None` for a unix socket.
    ///
    /// # Errors
    ///
    /// The system cannot report the address.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unnecessary_wraps,
            reason = "the `None` arm is the unix socket, which only unix compiles"
        )
    )]
    pub(super) fn tcp_addr(&self) -> Option<io::Result<std::net::SocketAddr>> {
        match self {
            #[cfg(not(windows))]
            Self::Tcp(listener) => Some(listener.local_addr()),
            Self::Handoff(incoming) => Some(incoming.local_addr()),
            #[cfg(unix)]
            Self::Unix(_) => None,
        }
    }
}

/// One listening socket per thread, all on `addr`.
///
/// Binding happens here, on the starting thread, so that an address that cannot be bound fails
/// the start before any thread exists, and so that an address with port `0` resolves to one port
/// that every listener then joins.
///
/// # Errors
///
/// The address cannot be bound, or a socket cannot be duplicated.
pub(super) fn bind(addr: &super::ListenAddr, threads: NonZeroUsize) -> io::Result<Bound> {
    match addr {
        #[cfg(windows)]
        super::ListenAddr::Tcp(addr) => {
            let (incoming, acceptor) = super::handoff::bind(*addr, threads.get())?;
            Ok(Bound {
                sockets: incoming.into_iter().map(Socket::Handoff).collect(),
                acceptor: Some(acceptor),
                udp: Vec::new(),
            })
        }
        #[cfg(not(windows))]
        super::ListenAddr::Tcp(addr) => Ok(Bound {
            sockets: bind_tcp(*addr, threads.get())?
                .into_iter()
                .map(Socket::Tcp)
                .collect(),
            acceptor: None,
            udp: Vec::new(),
        }),
        #[cfg(unix)]
        super::ListenAddr::Unix(path) => {
            let listener = super::bind_unix_socket(path)?;
            // The descriptors share one open socket, so connections queue once and whichever
            // thread is idle takes the next.
            let mut sockets = Vec::with_capacity(threads.get());
            for _ in 1..threads.get() {
                sockets.push(Socket::Unix(listener.try_clone()?));
            }
            sockets.push(Socket::Unix(listener));
            Ok(Bound {
                sockets,
                acceptor: None,
                udp: Vec::new(),
            })
        }
    }
}

/// The backlog asked of `listen(2)`; the system clamps it to its own limit.
pub(super) const BACKLOG: i32 = i32::MAX;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_tcp(addr: std::net::SocketAddr, threads: usize) -> io::Result<Vec<std::net::TcpListener>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let listener = |addr: std::net::SocketAddr| -> io::Result<std::net::TcpListener> {
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
        socket.bind(&addr.into())?;
        socket.listen(BACKLOG)?;
        Ok(socket.into())
    };

    let first = listener(addr)?;
    // Port 0 asked the system for a port; the other listeners join the one it chose.
    let addr = first.local_addr()?;
    let mut listeners = vec![first];
    for _ in 1..threads {
        listeners.push(listener(addr)?);
    }
    Ok(listeners)
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
fn bind_tcp(addr: std::net::SocketAddr, threads: usize) -> io::Result<Vec<std::net::TcpListener>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    // The address is bound once and shared. `SO_REUSEPORT` would not balance here: the BSDs and
    // macOS deliver every connection to one of the listeners.
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(BACKLOG)?;
    let listener: std::net::TcpListener = socket.into();
    let mut listeners = Vec::with_capacity(threads);
    for _ in 1..threads {
        listeners.push(listener.try_clone()?);
    }
    listeners.push(listener);
    Ok(listeners)
}

/// What every serving thread is started from.
#[derive(Debug)]
pub(super) struct Plan<F, W> {
    /// Builds a thread's router, on that thread.
    pub(super) factory: F,
    /// Wraps a thread's router for the platform hosting it; see [`super::serve_cores`].
    pub(super) wrap: W,
    /// Where `WorkerContext::wait_until` runs post-response work.
    pub(super) executor: Arc<AnyExecutor>,
    /// How many connections one thread keeps open before it refuses more. Applies to the default
    /// server and to TLS; [`Hyper`] on a cleartext listener has no such limit.
    pub(super) max_connections: NonZeroUsize,
    /// The server that answers cleartext TCP connections.
    pub(super) server: HttpServer,
    /// The acceptor, when the listener terminates TLS.
    pub(super) tls: Option<ServerTls>,
    /// HTTP/3, when it is served beside the TLS listener.
    pub(super) http3: Option<Http3Plan>,
}

/// What HTTP/3 needs beyond the sockets in [`Bound::udp`].
#[derive(Debug)]
pub(super) struct Http3Plan {
    /// The settings, certificate and key every thread's QUIC endpoint is built from.
    pub(super) http3: Http3,
    /// Points TLS TCP clients at the UDP port, on every response they get.
    pub(super) alt_svc: AltSvc,
}

/// The serving threads, once started.
#[derive(Debug)]
pub(super) struct Cores {
    /// How many threads were started; each reports on `ready` exactly once.
    pub(super) threads: usize,
    /// A thread reports `Ok` once it is accepting, or why it cannot. A channel that closes before
    /// every thread has reported means one of them died.
    pub(super) ready: Receiver<Result<(), RuntimeFailure>>,
    /// Closes when the last thread has exited.
    pub(super) exited: Receiver<Infallible>,
}

/// Start one serving thread for each of `bound.sockets`, and the acceptor that feeds them if
/// there is one.
///
/// Every thread stops accepting when `stop` closes, lets its open connections finish, and holds a
/// clone of `drain` for as long as it or any request on it is alive. It then keeps its runtime
/// running until `release` closes — once the process has drained — because the work it handed the
/// global executor, a post-response task, may still be sending on the connection pools that
/// runtime drives.
///
/// # Errors
///
/// A thread could not be created.
pub(super) fn start<F, A, W>(
    plan: Plan<F, W>,
    bound: Bound,
    stop: &Receiver<Infallible>,
    drain: &Sender<Infallible>,
    release: &Receiver<Infallible>,
) -> io::Result<Cores>
where
    F: EndpointFactory,
    A: Endpoint + Clone + 'static,
    W: Fn(F::Endpoint) -> Result<A, RuntimeFailure> + Send + Sync + 'static,
{
    let Bound {
        sockets,
        acceptor,
        udp,
    } = bound;
    let mut udp = udp.into_iter();
    let threads = sockets.len();
    let plan = Arc::new(plan);
    let (ready_tx, ready) = bounded(threads);
    // Nothing is sent: it closes once every thread has dropped its sender.
    let (exited_tx, exited) = bounded::<Infallible>(1);

    if let Some(acceptor) = acceptor {
        let stop = stop.clone();
        let exited = exited_tx.clone();
        thread::Builder::new()
            .name("skyzen-accept".to_owned())
            .spawn(move || {
                acceptor.run(&stop);
                drop(exited);
            })?;
    }

    for (index, socket) in sockets.into_iter().enumerate() {
        let plan = Arc::clone(&plan);
        let stop = stop.clone();
        let drain = drain.clone();
        let release = release.clone();
        let ready = ready_tx.clone();
        let exited = exited_tx.clone();
        let udp = udp.next().flatten();
        thread::Builder::new()
            .name(format!("skyzen-{index}"))
            .spawn(move || {
                work(&plan, socket, udp, stop, drain, &release, &ready);
                drop(exited);
            })?;
    }

    Ok(Cores {
        threads,
        ready,
        exited,
    })
}

/// The body of one serving thread: a compio runtime that serves until `stop` closes and runs until
/// `release` does.
fn work<F, A, W>(
    plan: &Arc<Plan<F, W>>,
    socket: Socket,
    udp: Option<std::net::UdpSocket>,
    stop: Receiver<Infallible>,
    drain: Sender<Infallible>,
    release: &Receiver<Infallible>,
    ready: &Sender<Result<(), RuntimeFailure>>,
) where
    F: EndpointFactory,
    A: Endpoint + Clone + 'static,
    W: Fn(F::Endpoint) -> Result<A, RuntimeFailure> + Send + Sync + 'static,
{
    let runtime = match Runtime::new() {
        Ok(runtime) => Rc::new(runtime),
        Err(error) => {
            let _ = ready.send_blocking(Err(error.into()));
            return;
        }
    };
    let executor = Rc::clone(&runtime);
    runtime.block_on(async {
        // The router is built here, on the thread that serves it, so it may hold anything.
        let endpoint = plan.factory.endpoint().await;
        let endpoint = match (plan.wrap)(endpoint) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                let _ = ready.send(Err(error)).await;
                return;
            }
        };
        let endpoint = WithContext {
            endpoint,
            context: WorkerContext::new(Arc::clone(&plan.executor), ShutdownGuard(drain)),
        };
        let mut h1 = H1::default().with_max_connections(plan.max_connections);
        // The QUIC endpoint, registered with this thread's runtime before anything reports
        // ready: a certificate QUIC cannot load is a failed start, not a TLS-only server.
        // Every thread's TLS responses advertise it, including one that does not run QUIC itself.
        if let Some(plan) = &plan.http3 {
            h1 = h1.with_alt_svc(plan.alt_svc.header_value());
        }
        let http3 = match (&plan.http3, udp) {
            (Some(plan), Some(udp)) => match plan.http3.server(udp) {
                Ok(server) => Some(server),
                Err(error) => {
                    let _ = ready.send(Err(RuntimeFailure::Http3(error))).await;
                    return;
                }
            },
            _ => None,
        };
        let h3_shutdown = {
            let stop = stop.clone();
            async move {
                // Resolves with an error once the sender is dropped.
                let _ = stop.recv().await;
            }
        };
        let shutdown = async move {
            // Resolves with an error once the sender is dropped.
            let _ = stop.recv().await;
        };

        // QUIC and TCP are independent: each drains its own connections once `stop` closes, and
        // the thread, which holds the drain token, ends when both have.
        let h3_endpoint = endpoint.clone();
        let h3 = async move {
            if let Some(server) = http3 {
                server.serve(h3_endpoint, h3_shutdown).await;
            }
        };
        let tcp = async {
            match socket {
                #[cfg(not(windows))]
                Socket::Tcp(listener) => match Compio.tcp_listener(listener) {
                    Ok(listener) => {
                        serve_tcp(plan, &h1, executor, listener, endpoint, shutdown, ready).await;
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error.into())).await;
                    }
                },
                Socket::Handoff(listener) => {
                    serve_tcp(plan, &h1, executor, listener, endpoint, shutdown, ready).await;
                }
                #[cfg(unix)]
                Socket::Unix(listener) => match Compio.unix_listener(listener) {
                    Ok(listener) => {
                        if announce(ready).await {
                            serve_cleartext(
                                plan.server,
                                &h1,
                                executor,
                                listener,
                                endpoint,
                                shutdown,
                            )
                            .await;
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error.into())).await;
                    }
                },
            }
        };
        futures_util::future::join(tcp, h3).await;
        // Both servers have returned, and with them the endpoint and the drain token it held.
        // Resolves with an error once the coordinator drops the sender.
        let _ = release.recv().await;
    });
}

/// Announce readiness, then serve `listener` over TCP: through TLS if the plan has it, else in
/// cleartext with the plan's server.
async fn serve_tcp<F, W, L, E>(
    plan: &Plan<F, W>,
    h1: &H1,
    executor: Rc<Runtime>,
    listener: L,
    endpoint: E,
    shutdown: impl Future<Output = ()> + 'static,
    ready: &Sender<Result<(), RuntimeFailure>>,
) where
    L: Listener<Stream = <Compio as Io>::TcpStream, Addr = std::net::SocketAddr>,
    E: Endpoint + Clone + 'static,
{
    if !announce(ready).await {
        return;
    }
    let listener = NoDelay(listener);
    match &plan.tls {
        // ALPN chooses the protocol on the decrypted stream: `h2` is hyper's, anything else is
        // HTTP/1.1 served by the plan's selected server.
        Some(tls) => {
            super::tls::serve(
                executor,
                listener,
                endpoint,
                shutdown,
                tls.clone(),
                plan.max_connections,
                Protocols {
                    server: plan.server,
                    h1: h1.clone(),
                    alt_svc: plan.http3.as_ref().map(|plan| plan.alt_svc.clone()),
                },
            )
            .await;
        }
        None => serve_cleartext(plan.server, h1, executor, listener, endpoint, shutdown).await,
    }
}

/// A TCP listener whose connections send every write at once. With Nagle's algorithm left on, a
/// response's last short segment waits for the client's delayed ACK, about 40 ms on Linux.
#[derive(Debug)]
struct NoDelay<L>(L);

impl<L> Listener for NoDelay<L>
where
    L: Listener,
    L::Stream: TcpOptions,
{
    type Stream = L::Stream;
    type Addr = L::Addr;

    async fn accept(&self) -> io::Result<(Self::Stream, Self::Addr)> {
        let (stream, peer) = self.0.accept().await?;
        stream.set_nodelay(true)?;
        Ok((stream, peer))
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Serve `listener`, TCP or unix, in cleartext with the chosen server.
async fn serve_cleartext<L, E>(
    server: HttpServer,
    h1: &H1,
    executor: Rc<Runtime>,
    listener: L,
    endpoint: E,
    shutdown: impl Future<Output = ()> + 'static,
) where
    L: Listener,
    L::Stream: Split + IntoPoll + 'static,
    <L::Stream as IntoPoll>::Poll: 'static,
    <L::Stream as Split>::WriteHalf: SendFile,
    L::Addr: PeerAddress,
    E: Endpoint + Clone + 'static,
{
    match server {
        HttpServer::H1 => {
            h1.serve_listener(Compio, executor, listener, endpoint, shutdown)
                .await;
        }
        HttpServer::Hyper => {
            Hyper
                .serve_listener(Compio, executor, listener, endpoint, shutdown)
                .await;
        }
    }
}

/// Report that this thread is accepting. `false` when nobody is waiting for the report any more.
async fn announce(ready: &Sender<Result<(), RuntimeFailure>>) -> bool {
    let reported = ready.send(Ok(())).await.is_ok();
    if !reported {
        error!("the runtime stopped waiting for this serving thread to start");
    }
    reported
}

/// An endpoint that hands every request the [`WorkerContext`] of the thread serving it.
///
/// The context carries a drain token, so a request in flight, and any post-response work it
/// registers, keeps graceful shutdown waiting.
#[derive(Debug, Clone)]
struct WithContext<E> {
    endpoint: E,
    context: WorkerContext,
}

impl<E: Endpoint> Endpoint for WithContext<E> {
    type Error = E::Error;

    fn respond(
        &mut self,
        request: &mut Request,
    ) -> impl core::future::Future<Output = Result<Response, Self::Error>> {
        request.extensions_mut().insert(self.context.clone());
        self.endpoint.respond(request)
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use std::{collections::BTreeSet, num::NonZeroUsize, sync::Arc};

    use async_channel::bounded;
    use executor_core::{smol::SmolGlobal, AnyExecutor};
    use http_kit::{error::BoxHttpError, Body};

    use super::{bind, start, Bound, Cores, Plan, Socket};
    use crate::runtime::native::handoff;
    use crate::{
        runtime::native::{HttpServer, ListenAddr},
        Endpoint, Request, Response,
    };

    /// Serving threads that have all reported ready, and the handles that stop them.
    struct Serving {
        stop: async_channel::Sender<Infallible>,
        drain: async_channel::Sender<Infallible>,
        drained: async_channel::Receiver<Infallible>,
        release: async_channel::Sender<Infallible>,
        cores: Cores,
    }

    impl Serving {
        fn start(bound: Bound, server: HttpServer) -> Self {
            let (stop, stopping) = bounded::<Infallible>(1);
            let (drain, drained) = bounded::<Infallible>(1);
            let (release, released) = bounded::<Infallible>(1);
            let plan = Plan {
                factory: || async { ServingThread },
                wrap: Ok,
                executor: Arc::new(AnyExecutor::new(SmolGlobal)),
                max_connections: NonZeroUsize::new(64).unwrap(),
                server,
                tls: None,
                http3: None,
            };
            let cores =
                start(plan, bound, &stopping, &drain, &released).expect("start the threads");
            for _ in 0..cores.threads {
                cores
                    .ready
                    .recv_blocking()
                    .expect("a thread reports")
                    .expect("and is serving");
            }
            Self {
                stop,
                drain,
                drained,
                release,
                cores,
            }
        }

        /// Stop every thread, wait until each has released its drain token, then let them exit
        /// and wait until each has.
        fn stop(self) {
            drop(self.stop);
            drop(self.drain);
            assert!(self.drained.recv_blocking().is_err());
            drop(self.release);
            assert!(self.cores.exited.recv_blocking().is_err());
        }
    }

    /// The body of a `Connection: close` response over `stream`.
    fn exchange(mut stream: impl std::io::Read + std::io::Write) -> String {
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .expect("write the request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        response
            .rsplit("\r\n\r\n")
            .next()
            .expect("a response body")
            .to_owned()
    }

    /// Answers with the name of the thread that served the request.
    #[derive(Clone)]
    struct ServingThread;

    impl Endpoint for ServingThread {
        type Error = BoxHttpError;

        fn respond(
            &mut self,
            _request: &mut Request,
        ) -> impl core::future::Future<Output = Result<Response, Self::Error>> {
            let thread = std::thread::current();
            core::future::ready(Ok(Response::new(Body::from(
                thread.name().unwrap_or("unnamed").to_owned(),
            ))))
        }
    }

    /// Four serving threads on one address, and requests that land on more than one of them.
    ///
    /// Each connection goes to the listener the kernel picks, so asking for it over many
    /// connections is what shows the work spreads; a single serving thread would answer every
    /// one under the same name.
    #[test]
    fn connections_land_on_more_than_one_runtime_thread() {
        // Both servers a cleartext listener can be given run through the same threads.
        for server in [HttpServer::H1, HttpServer::Hyper] {
            connections_spread_over_threads(server);
        }
    }

    fn connections_spread_over_threads(server: HttpServer) {
        let threads = NonZeroUsize::new(4).unwrap();
        let bound = bind(&ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()), threads).expect("bind");
        let addr = bound.sockets[0]
            .tcp_addr()
            .expect("a TCP address binds TCP sockets")
            .expect("local addr");
        let serving = Serving::start(bound, server);
        assert_eq!(serving.cores.threads, 4);
        assert_spread(addr, server, serving);
    }

    /// The same, through the acceptor that Windows uses in place of a shared socket.
    #[test]
    fn dealt_connections_land_on_more_than_one_runtime_thread() {
        for server in [HttpServer::H1, HttpServer::Hyper] {
            let (incoming, acceptor) =
                handoff::bind("127.0.0.1:0".parse().unwrap(), 4).expect("bind");
            let bound = Bound {
                sockets: incoming.into_iter().map(Socket::Handoff).collect(),
                acceptor: Some(acceptor),
                udp: Vec::new(),
            };
            let addr = bound.sockets[0]
                .tcp_addr()
                .expect("a TCP address")
                .expect("local addr");
            let serving = Serving::start(bound, server);
            assert_spread(addr, server, serving);
        }
    }

    fn assert_spread(addr: std::net::SocketAddr, server: HttpServer, serving: Serving) {
        let served_by: BTreeSet<String> = (0..128)
            .map(|_| exchange(std::net::TcpStream::connect(addr).expect("connect")))
            .collect();

        serving.stop();

        assert!(
            served_by.len() > 1,
            "every request landed on one runtime thread under {server:?}: {served_by:?}"
        );
        assert!(served_by.iter().all(|name| name.starts_with("skyzen-")));
    }

    /// A unix socket is served by the same threads under either server, and the file goes away
    /// with the socket's owner, not with a connection.
    #[test]
    #[cfg(unix)]
    fn a_unix_socket_is_served_by_the_serving_threads() {
        for (index, server) in [HttpServer::H1, HttpServer::Hyper].into_iter().enumerate() {
            let path = std::env::temp_dir()
                .join(format!("skyzen-cores-{}-{index}.sock", std::process::id()));
            let sockets = bind(
                &ListenAddr::Unix(path.clone()),
                NonZeroUsize::new(2).unwrap(),
            )
            .expect("bind the unix socket");
            let serving = Serving::start(sockets, server);

            let body = exchange(std::os::unix::net::UnixStream::connect(&path).expect("connect"));

            serving.stop();
            std::fs::remove_file(&path).expect("the socket file is ours to remove");
            assert!(body.starts_with("skyzen-"), "{server:?}: {body}");
        }
    }

    /// A TCP connection is served with Nagle's algorithm off.
    #[test]
    fn accepted_tcp_connections_send_without_delay() {
        use http_kit_io::{compio::Compio, Io, Listener, TcpOptions};

        executor_core::compio::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                let listener = super::NoDelay(
                    Compio
                        .bind_tcp("127.0.0.1:0".parse().unwrap())
                        .expect("bind loopback"),
                );
                let addr = listener.local_addr().expect("local addr");
                let (accepted, client) =
                    futures_util::future::join(listener.accept(), Compio.connect(addr)).await;
                let _client = client.expect("connect");
                let (stream, _peer) = accepted.expect("accept");
                assert!(stream.nodelay().expect("read TCP_NODELAY"));
            });
    }
}
