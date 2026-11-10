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

use core::convert::Infallible;
use std::{io, num::NonZeroUsize, rc::Rc, sync::Arc, thread};

use async_channel::{bounded, Receiver, Sender};
use executor_core::{compio::Runtime, AnyExecutor};
use http_kit_io::{compio::Compio, Io, Split};
use skyzen_core::EndpointFactory;
use skyzen_h1::{Accept, H1};
use tracing::error;

use super::RuntimeFailure;
use crate::{
    runtime::{context::ShutdownGuard, WorkerContext},
    Endpoint, Request, Response,
};

/// A bound, listening socket waiting for the thread that will serve it.
#[derive(Debug)]
pub(super) enum Socket {
    /// A TCP listener.
    Tcp(std::net::TcpListener),
    /// A filesystem unix socket.
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
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
pub(super) fn bind(addr: &super::ListenAddr, threads: NonZeroUsize) -> io::Result<Vec<Socket>> {
    match addr {
        super::ListenAddr::Tcp(addr) => Ok(bind_tcp(*addr, threads.get())?
            .into_iter()
            .map(Socket::Tcp)
            .collect()),
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
            Ok(sockets)
        }
    }
}

/// The backlog asked of `listen(2)`; the system clamps it to its own limit.
const BACKLOG: i32 = i32::MAX;

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

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn bind_tcp(addr: std::net::SocketAddr, threads: usize) -> io::Result<Vec<std::net::TcpListener>> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    // The address is bound once and shared. `SO_REUSEPORT` would not balance here: the BSDs and
    // macOS deliver every connection to one of the listeners, and Windows has no such option
    // (its `SO_REUSEADDR` lets another process take the port over).
    #[cfg(not(windows))]
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
    /// How many connections one thread keeps open before it refuses more.
    pub(super) max_connections: NonZeroUsize,
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

/// Start one serving thread for each of `sockets`.
///
/// Every thread stops accepting when `stop` closes, lets its open connections finish, and holds a
/// clone of `drain` for as long as it or any request on it is alive.
///
/// # Errors
///
/// A thread could not be created.
pub(super) fn start<F, A, W>(
    plan: Plan<F, W>,
    sockets: Vec<Socket>,
    stop: &Receiver<Infallible>,
    drain: &Sender<Infallible>,
) -> io::Result<Cores>
where
    F: EndpointFactory,
    A: Endpoint + Clone + 'static,
    W: Fn(F::Endpoint) -> Result<A, RuntimeFailure> + Send + Sync + 'static,
{
    let threads = sockets.len();
    let plan = Arc::new(plan);
    let (ready_tx, ready) = bounded(threads);
    // Nothing is sent: it closes once every thread has dropped its sender.
    let (exited_tx, exited) = bounded::<Infallible>(1);

    for (index, socket) in sockets.into_iter().enumerate() {
        let plan = Arc::clone(&plan);
        let stop = stop.clone();
        let drain = drain.clone();
        let ready = ready_tx.clone();
        let exited = exited_tx.clone();
        thread::Builder::new()
            .name(format!("skyzen-{index}"))
            .spawn(move || {
                work(&plan, socket, stop, drain, &ready);
                drop(exited);
            })?;
    }

    Ok(Cores {
        threads,
        ready,
        exited,
    })
}

/// The body of one serving thread: a compio runtime that serves until `stop` closes.
fn work<F, A, W>(
    plan: &Arc<Plan<F, W>>,
    socket: Socket,
    stop: Receiver<Infallible>,
    drain: Sender<Infallible>,
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
        let h1 = H1::default().with_max_connections(plan.max_connections);
        let shutdown = async move {
            // Resolves with an error once the sender is dropped.
            let _ = stop.recv().await;
        };

        match socket {
            Socket::Tcp(listener) => match Compio.tcp_listener(listener) {
                Ok(listener) => {
                    serve(&h1, executor, listener, endpoint, shutdown, ready).await;
                }
                Err(error) => {
                    let _ = ready.send(Err(error.into())).await;
                }
            },
            #[cfg(unix)]
            Socket::Unix(listener) => match super::unix::Listener::from_std(listener) {
                Ok(listener) => {
                    serve(&h1, executor, listener, endpoint, shutdown, ready).await;
                }
                Err(error) => {
                    let _ = ready.send(Err(error.into())).await;
                }
            },
        }
    });
}

/// Report that this thread is accepting, then serve until `shutdown`.
async fn serve<L, E>(
    h1: &H1,
    executor: Rc<Runtime>,
    listener: L,
    endpoint: E,
    shutdown: impl core::future::Future<Output = ()> + 'static,
    ready: &Sender<Result<(), RuntimeFailure>>,
) where
    L: Accept,
    <L::Stream as Split>::WriteHalf: http_kit_io::SendFile,
    E: Endpoint + Clone + 'static,
{
    if ready.send(Ok(())).await.is_err() {
        error!("the runtime stopped waiting for this serving thread to start");
        return;
    }
    h1.serve_accepting(Compio, executor, listener, endpoint, shutdown)
        .await;
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

    use super::{bind, start, Cores, Plan, Socket};
    use crate::{runtime::native::ListenAddr, Endpoint, Request, Response};

    /// Serving threads that have all reported ready, and the handles that stop them.
    struct Serving {
        stop: async_channel::Sender<Infallible>,
        drain: async_channel::Sender<Infallible>,
        drained: async_channel::Receiver<Infallible>,
        cores: Cores,
    }

    impl Serving {
        fn start(sockets: Vec<Socket>) -> Self {
            let (stop, stopping) = bounded::<Infallible>(1);
            let (drain, drained) = bounded::<Infallible>(1);
            let plan = Plan {
                factory: || async { ServingThread },
                wrap: Ok,
                executor: Arc::new(AnyExecutor::new(SmolGlobal)),
                max_connections: NonZeroUsize::new(64).unwrap(),
            };
            let cores = start(plan, sockets, &stopping, &drain).expect("start the threads");
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
                cores,
            }
        }

        /// Stop every thread and wait until each has exited and released its drain token.
        fn stop(self) {
            drop(self.stop);
            drop(self.drain);
            assert!(self.cores.exited.recv_blocking().is_err());
            assert!(self.drained.recv_blocking().is_err());
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
        let threads = NonZeroUsize::new(4).unwrap();
        let sockets =
            bind(&ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()), threads).expect("bind");
        let Socket::Tcp(first) = &sockets[0] else {
            unreachable!("a TCP address binds TCP sockets")
        };
        let addr = first.local_addr().expect("local addr");

        let serving = Serving::start(sockets);
        assert_eq!(serving.cores.threads, 4);

        let served_by: BTreeSet<String> = (0..128)
            .map(|_| exchange(std::net::TcpStream::connect(addr).expect("connect")))
            .collect();

        serving.stop();

        assert!(
            served_by.len() > 1,
            "every request landed on one runtime thread: {served_by:?}"
        );
        assert!(served_by.iter().all(|name| name.starts_with("skyzen-")));
    }

    /// A unix socket is served by the same threads and the same HTTP/1.1 loop, and the file goes
    /// away with the socket's owner, not with a connection.
    #[test]
    #[cfg(unix)]
    fn a_unix_socket_is_served_by_the_serving_threads() {
        let path = std::env::temp_dir().join(format!("skyzen-cores-{}.sock", std::process::id()));
        let sockets = bind(
            &ListenAddr::Unix(path.clone()),
            NonZeroUsize::new(2).unwrap(),
        )
        .expect("bind the unix socket");
        let serving = Serving::start(sockets);

        let body = exchange(std::os::unix::net::UnixStream::connect(&path).expect("connect"));

        serving.stop();
        std::fs::remove_file(&path).expect("the socket file is ours to remove");
        assert!(body.starts_with("skyzen-"), "{body}");
    }
}
