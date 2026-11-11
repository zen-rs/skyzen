use core::future::Future;

use executor_core::LocalExecutor;
use http_kit::Endpoint;
use http_kit_io::Listen;

/// An HTTP protocol implementation, serving one thread's share of an application.
///
/// A server runs on the thread that calls [`Server::serve`]: it accepts on the listeners it is
/// given, spawns every connection through `executor`, and answers each request with a clone of
/// `endpoint`. Nothing it drives has to be `Send` — not the endpoint, not its middleware or their
/// state, not the futures they return — so a router may hold an `Rc`.
///
/// A multi-threaded deployment calls `serve` once per thread, each with a listener of its own
/// (`SO_REUSEPORT`) and an endpoint built on that thread by a `Send` factory. What crosses threads
/// is the factory and the services it captures, never a router.
///
/// `I` is the I/O driver the listeners belong to — `http_kit_io::compio::Compio`,
/// `http_kit_io::tokio::Tokio` or `http_kit_io::async_io::AsyncIo` — named by the one capability
/// every server needs from it, binding a listener. An implementation asks for the others it uses
/// (a `Timer` for its deadlines) on its own impl.
pub trait Server<I: Listen> {
    /// What this server accepts work on.
    ///
    /// A protocol over TCP takes `I::Listener`, one over QUIC takes a datagram socket, and a server
    /// that speaks both takes the pair — so a server can never be handed a transport it does not
    /// speak.
    type Listeners;

    /// Serve until `shutdown` resolves.
    ///
    /// Once it does, the server stops accepting, lets every open connection finish the exchange in
    /// flight, and resolves when the last one has closed. A caller with a grace period races this
    /// future against its deadline. Connection and protocol errors are logged through `tracing`
    /// and end only the connection they occurred on.
    fn serve<Ex, E>(
        &self,
        io: I,
        executor: Ex,
        listeners: Self::Listeners,
        endpoint: E,
        shutdown: impl Future<Output = ()> + 'static,
    ) -> impl Future<Output = ()>
    where
        Ex: LocalExecutor + 'static,
        E: Endpoint + Clone + 'static;
}

/// Builds the endpoint one serving thread answers with.
///
/// The factory is `Send + Sync`, so it can be handed to every thread a runtime starts; what it
/// builds is not, so each thread calls it once, on itself, and keeps the result. The services it
/// captures are built once and shared — they are `Send + Sync` — while the router and its
/// per-thread state are built anew on each thread and never leave it.
///
/// Implemented for every `Fn() -> impl Future<Output = E> + Send + Sync`.
pub trait EndpointFactory: Send + Sync + 'static {
    /// The endpoint each thread serves.
    type Endpoint: Endpoint + Clone + 'static;

    /// Build this thread's endpoint.
    fn endpoint(&self) -> impl Future<Output = Self::Endpoint>;
}

impl<F, Fut, E> EndpointFactory for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = E>,
    E: Endpoint + Clone + 'static,
{
    type Endpoint = E;

    fn endpoint(&self) -> impl Future<Output = E> {
        self()
    }
}
