use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll},
};

use crate::{
    extract::PeerAddr,
    routing::ServedRoutes,
    runtime::{
        azure,
        consumer::{ConsumerFatal, ConsumerSet},
        context::ShutdownGuard,
        WorkerContext,
    },
    Endpoint,
};
use async_channel::{bounded, Receiver, Sender};
use async_net::TcpListener;
use core::convert::Infallible;
use executor_core::{
    smol::SmolGlobal, try_init_global_executor, AnyExecutor, Executor as CoreExecutor, Task,
};
use futures_util::{future::FutureExt, stream::MapOk, StreamExt, TryStreamExt};
use http_body_util::{BodyDataStream, StreamBody};
use http_kit::{
    error::BoxHttpError,
    utils::{AsyncRead, AsyncReadExt, AsyncWrite},
    BodyError,
};
use hyper::{
    body::{Frame, Incoming},
    server::conn::{http1, http2},
    service::Service,
};
use skyzen_hyper::AsyncIoTimer;
use tracing::{debug, error, info, warn};
use tracing_log::log::LevelFilter as LogLevelFilter;
use tracing_subscriber::EnvFilter;

type BoxFuture<T> = Pin<Box<dyn Send + Future<Output = T> + 'static>>;

struct HyperExecutor<E>(Arc<E>);

impl<E> Clone for HyperExecutor<E> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<E> std::fmt::Debug for HyperExecutor<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperExecutor").finish_non_exhaustive()
    }
}

impl<Fut, E> hyper::rt::Executor<Fut> for HyperExecutor<E>
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
    E: CoreExecutor + 'static,
{
    fn execute(&self, fut: Fut) {
        self.0.spawn(fut).detach();
    }
}

struct ConnectionWrapper<C>(C);

impl<C: Unpin + AsyncRead> hyper::rt::Read for ConnectionWrapper<C> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;

        // SAFETY: `buf.as_mut()` gives a `&mut [MaybeUninit<u8>]`. `AsyncRead` expects
        // initialized memory, so we first zero every slot; only then is viewing the slice as
        // `&mut [u8]` sound (a cast without initialization would be UB). We advance the buffer
        // by the number of bytes written to maintain correctness.
        let buffer = unsafe {
            let unfilled = buf.as_mut();
            for byte in unfilled.iter_mut() {
                byte.write(0);
            }
            &mut *(ptr::from_mut(unfilled) as *mut [u8])
        };

        match Pin::new(inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(n)) => {
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

#[derive(Debug)]
struct Prefixed<C> {
    buffer: Vec<u8>,
    pos: usize,
    inner: C,
}

impl<C> Prefixed<C> {
    const fn new(inner: C, buffer: Vec<u8>) -> Self {
        Self {
            buffer,
            pos: 0,
            inner,
        }
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

/// Initialize the tracing subscriber + color-eyre once per process.
/// # Panics
/// If the subscriber fails to initialize.
pub fn init_logging() {
    use std::sync::Once;

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(error) = color_eyre::install() {
            eprintln!("failed to install color-eyre: {error}");
        }

        let _ = tracing_log::LogTracer::builder()
            .with_max_level(LogLevelFilter::Trace)
            .init();

        let env_filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new("info"))
            .expect("failed to build env filter");

        if tracing::dispatcher::has_been_set() {
            return;
        }

        if let Err(error) = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_target(true)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_file(false)
            .with_line_number(false)
            .event_format(
                tracing_subscriber::fmt::format()
                    .with_level(true)
                    .with_target(true)
                    .compact(),
            )
            .try_init()
        {
            // Another subscriber was already installed (likely by a test harness),
            // so we ignore the error to avoid noisy stderr output.
            tracing::debug!("tracing subscriber already initialized: {error:?}");
        }
    });
}

/// A listener address a command-line flag asked for.
///
/// Which flag it came from matters: a serverless host that dictates the port (Azure Functions
/// does) outranks the defaults and outranks `--host`/`--port`, which only adjust them — but not an
/// explicit `--listen`, where the operator named the whole socket and meant it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenOverride {
    /// The address to bind.
    pub addr: SocketAddr,
    /// Whether `--listen`/`--addr` named this socket outright.
    pub explicit: bool,
}

/// Parse CLI overrides such as `--addr`/`--port` and return the resulting listen address.
///
/// Returns `None` when no valid override was supplied; the caller then falls back to the
/// `SKYZEN_ADDRESS` environment variable or the built-in default (see [`server_addr`]). Invalid
/// values are logged and ignored rather than mutating any global state.
#[must_use]
pub fn apply_cli_overrides(args: impl IntoIterator<Item = String>) -> Option<ListenOverride> {
    let mut args = args.into_iter();
    let _ = args.next(); // binary name
    let mut listen = None;
    let mut host = None;
    let mut port = None;

    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--listen=") {
            listen = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--addr=") {
            listen = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--host=") {
            host = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--port=") {
            port = Some(value.to_owned());
        } else {
            match arg.as_str() {
                "--listen" | "--addr" => {
                    if let Some(value) = args.next() {
                        listen = Some(value);
                    }
                }
                "--host" => {
                    if let Some(value) = args.next() {
                        host = Some(value);
                    }
                }
                "--port" | "-p" => {
                    if let Some(value) = args.next() {
                        port = Some(value);
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(addr) = listen {
        return match addr.parse::<SocketAddr>() {
            Ok(socket) => {
                info!("Configured listener address via CLI: {socket}");
                Some(ListenOverride {
                    addr: socket,
                    explicit: true,
                })
            }
            Err(error) => {
                warn!("Ignoring invalid --listen address `{addr}`: {error}");
                None
            }
        };
    }

    if host.is_none() && port.is_none() {
        return None;
    }

    let mut candidate = server_addr();
    if let Some(host) = host {
        match host.parse::<IpAddr>() {
            Ok(ip) => candidate.set_ip(ip),
            Err(error) => {
                warn!("Ignoring invalid --host `{host}`: {error}");
                return None;
            }
        }
    }
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(value) => candidate.set_port(value),
            Err(error) => {
                warn!("Ignoring invalid --port `{port}`: {error}");
                return None;
            }
        }
    }

    info!("Configured listener address via CLI: {candidate}");
    Some(ListenOverride {
        addr: candidate,
        explicit: false,
    })
}

fn shutdown_signal() -> Receiver<()> {
    let (tx, rx) = bounded(1);
    if let Err(error) = ctrlc::set_handler(move || {
        let _ = tx.try_send(());
    }) {
        warn!("Unable to install Ctrl+C handler: {error}");
    }
    rx
}

/// How long outstanding work is given to finish after the accept loop stops.
///
/// A request still streaming a response — or a queue consumer still settling the batch it is
/// holding — when the deadline elapses is severed; the runtime says so in its final log line
/// rather than claiming a graceful shutdown.
pub const SHUTDOWN_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(30);

/// What the accept loop had left to clean up when it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shutdown {
    /// Connections and consumer batches still in flight when the grace period elapsed, and
    /// therefore cut off.
    pub severed: usize,
}

/// Why the runtime stopped before it could shut down on its own terms.
#[derive(Debug)]
enum RuntimeFailure {
    /// The listener could not be bound, or the accept loop failed.
    Listener(std::io::Error),
    /// A declared queue consumer cannot run at all against the backend it was pointed at.
    QueueConsumer(ConsumerFatal),
    /// The Azure Functions triggers cannot be mounted over this application.
    Mount(azure::MountError),
}

impl From<std::io::Error> for RuntimeFailure {
    fn from(error: std::io::Error) -> Self {
        Self::Listener(error)
    }
}

impl std::fmt::Display for RuntimeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Listener(error) => write!(f, "the listener failed: {error}"),
            Self::QueueConsumer(ConsumerFatal { queue, reason }) => {
                write!(f, "the queue consumer for `{queue}` cannot run: {reason}")
            }
            Self::Mount(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RuntimeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Listener(error) => Some(error),
            Self::Mount(error) => Some(error),
            Self::QueueConsumer(_) => None,
        }
    }
}

/// Everything `#[skyzen::main]` settles before the runtime starts.
///
/// A struct rather than a widening argument list: what the runtime needs to know grows with every
/// platform it learns to serve, and each of those is a compile-time fact read out of `Skyzen.toml`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchOptions {
    /// The address a command-line flag asked for, if any.
    pub listen: Option<ListenOverride>,
    /// The `[[azure.queue_triggers]]` entries this application declares.
    ///
    /// Empty for everything that is not an Azure Functions app, which is the only case where they
    /// mean anything.
    pub azure_queue_triggers: &'static [azure::QueueTrigger],
}

/// Where this process is running, as told by the environment its host set up.
///
/// Detected rather than declared: the same binary is a server, a Lambda function and a Functions
/// custom handler, and nothing in the source says which — so the runtime asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    /// An ordinary server, binding a listener of its own.
    Server,
    /// AWS Lambda, which hands out invocations over the runtime API.
    Lambda,
    /// The Azure Functions host, which expects a web server on the port it chose.
    AzureFunctions {
        /// The port the host is waiting on.
        port: u16,
    },
}

/// Set by the AWS Lambda execution environment, and by nothing else.
const LAMBDA_RUNTIME_API_ENV: &str = "AWS_LAMBDA_RUNTIME_API";

impl Platform {
    /// Read the environment to see what is hosting this process.
    ///
    /// A malformed `FUNCTIONS_CUSTOMHANDLER_PORT` is not a reason to fall back to a port of our
    /// own choosing: the host would then wait forever on the port it picked, so the value is
    /// reported and the process refuses to pretend.
    fn detect() -> Result<Self, StartupError> {
        if std::env::var_os(LAMBDA_RUNTIME_API_ENV).is_some() {
            return Ok(Self::Lambda);
        }

        std::env::var(azure::CUSTOM_HANDLER_PORT_ENV).map_or(Ok(Self::Server), |port| {
            port.trim()
                .parse::<u16>()
                .map(|port| Self::AzureFunctions { port })
                .map_err(|_| StartupError::CustomHandlerPort { value: port })
        })
    }
}

/// A reason the runtime refused to start at all.
#[derive(Debug)]
enum StartupError {
    /// The process is inside Lambda but was not built with the adapter that speaks to it.
    #[cfg(not(feature = "lambda"))]
    LambdaFeatureMissing,
    /// The Functions host named a port that is not a port.
    CustomHandlerPort {
        /// What it said instead.
        value: String,
    },
    /// The Lambda adapter stopped.
    #[cfg(feature = "lambda")]
    Lambda(String),
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(not(feature = "lambda"))]
            Self::LambdaFeatureMissing => write!(
                f,
                "this process is running inside AWS Lambda ({LAMBDA_RUNTIME_API_ENV} is set), but \
                 it was built without Skyzen's Lambda adapter. Add `features = [\"lambda\"]` to \
                 the skyzen dependency and redeploy"
            ),
            Self::CustomHandlerPort { value } => write!(
                f,
                "the Azure Functions host set {} to {value:?}, which is not a port number",
                azure::CUSTOM_HANDLER_PORT_ENV
            ),
            #[cfg(feature = "lambda")]
            Self::Lambda(error) => write!(f, "the AWS Lambda runtime stopped: {error}"),
        }
    }
}

impl std::error::Error for StartupError {}

/// Build the executor, serve the provided endpoint over Hyper, and run the declared queue
/// consumers beside it.
///
/// `factory` produces the endpoint together with its [`ConsumerSet`] — `()` for an application
/// that declares no `[[native.queue_consumer]]` — so both are built from the same service
/// instances rather than from two independent connections to one backend.
///
/// # Where this ends up running
///
/// The environment decides, because the same binary serves all three:
///
/// - **AWS Lambda** (`AWS_LAMBDA_RUNTIME_API` is set) hands over to `skyzen-lambda` before any
///   listener is bound. Without the `lambda` feature the process refuses to start rather than
///   binding a TCP port nothing inside Lambda can reach.
/// - **Azure Functions** (`FUNCTIONS_CUSTOMHANDLER_PORT` is set) binds the port the host chose and
///   mounts the declared queue triggers ahead of the application's routes.
/// - Anything else binds the address the flags, `SKYZEN_ADDRESS` or the default asked for.
///
/// Under either serverless host the declared `[[native.queue_consumer]]` polling loops do *not*
/// run: the platform owns delivery there, and a loop polling a queue inside a function that scales
/// to zero would take messages nothing is waiting to process.
///
/// `on_shutdown` does not run on the Lambda path either. Lambda freezes and later discards an
/// execution environment without telling the process, so there is no moment at which a cleanup
/// hook could be called and be believed.
///
/// After `Ctrl+C` the listener stops accepting and the consumers stop initiating receives;
/// outstanding connections and in-flight batches are awaited for up to
/// [`SHUTDOWN_GRACE_PERIOD`], and only then does `on_shutdown` run — so a cleanup hook observes a
/// server with no requests and no batches still in flight.
///
/// A consumer that can never receive at all, and a listener that cannot be bound, both end the
/// process with a non-zero status once `on_shutdown` has run: a misconfigured deployment should
/// be restarted by its supervisor, not left running half-alive.
///
/// # Panics
///
/// Panics if the global executor fails to initialize.
pub fn launch<Fut, E, C, Hook, HookFut>(
    options: LaunchOptions,
    factory: impl FnOnce() -> Fut,
    on_shutdown: Hook,
) where
    Fut: Future<Output = (E, C)> + Send + 'static,
    E: Endpoint + ServedRoutes + Clone + Send + Sync + 'static,
    C: ConsumerSet,
    Hook: FnOnce() -> HookFut,
    HookFut: Future<Output = ()>,
{
    let platform = match Platform::detect() {
        Ok(platform) => platform,
        Err(error) => {
            error!("Skyzen cannot start: {error}");
            std::process::exit(1);
        }
    };

    if platform == Platform::Lambda {
        // Lambda's runtime is Tokio's and the adapter owns it, so this path never touches the
        // executor, the listener or the shutdown drain below.
        // `run_on_lambda` never returns `Ok`: it either hands over for the life of the process
        // or explains why it could not.
        let Err(error) = run_on_lambda(options, factory);
        error!("Skyzen cannot start: {error}");
        std::process::exit(1);
    }

    let executor = SmolGlobal;
    if try_init_global_executor(executor).is_err() {
        debug!("Global executor already initialized; reusing existing instance");
    }

    let failed = smol::block_on(async move {
        tracing::info!("Skyzen application starting up");

        let (endpoint, consumers) = factory().await;
        let outcome = match platform {
            Platform::Lambda => unreachable!("handed over before the executor was built"),
            Platform::Server => {
                let addr = options
                    .listen
                    .map_or_else(server_addr, |listen| listen.addr);
                run_server(executor, endpoint, consumers, addr).await
            }
            Platform::AzureFunctions { port } => {
                serve_functions_host(executor, endpoint, consumers, &options, port).await
            }
        };
        let failed = match outcome {
            Ok(Shutdown { severed: 0 }) => {
                info!("Skyzen server shut down gracefully");
                false
            }
            Ok(Shutdown { severed }) => {
                warn!(
                    in_flight = severed,
                    grace_period_secs = SHUTDOWN_GRACE_PERIOD.as_secs(),
                    "Shutdown deadline elapsed with work still in flight; it was severed"
                );
                false
            }
            Err(error) => {
                error!("Skyzen server terminated: {error}");
                true
            }
        };

        on_shutdown().await;
        failed
    });

    if failed {
        std::process::exit(1);
    }
}

/// Wait for every connection task to finish, or for the grace period to elapse.
///
/// Each connection task holds a clone of the channel's sender, so `recv` resolves — with a
/// closed-channel error, since nothing is ever sent — exactly when the last one is dropped. The
/// senders still alive at the deadline are the connections being cut off.
async fn drain_connections(
    connections: &Receiver<Infallible>,
    grace_period: std::time::Duration,
) -> usize {
    let drained = std::pin::pin!(connections.recv());
    let deadline = std::pin::pin!(async_io::Timer::after(grace_period));

    match futures_util::future::select(drained, deadline).await {
        futures_util::future::Either::Left(..) => 0,
        futures_util::future::Either::Right(..) => connections.sender_count(),
    }
}

/// The channels tying the accept loop to the queue consumers it started.
///
/// Dropping the whole thing is what tells every consumer slot to stop initiating receives, so the
/// shutdown path cannot forget one of the two halves.
#[derive(Debug)]
struct ConsumerChannels {
    /// Watched by every consumer slot; closing it stops them.
    _stop: Sender<Infallible>,
    /// Where a consumer that can never receive reports itself.
    fatal_rx: Receiver<ConsumerFatal>,
    /// The accept loop's own sender, kept alive so [`Self::fatal_rx`] stays open: a closed
    /// channel would resolve immediately and read as a failure for an application that declares
    /// no consumers at all.
    _fatal_tx: Sender<ConsumerFatal>,
}

impl ConsumerChannels {
    /// Start `consumers` on `executor`, each slot holding a clone of the drain `guard`.
    fn start<Exec: CoreExecutor + 'static, C: ConsumerSet>(
        consumers: C,
        executor: &Exec,
        guard: &Sender<Infallible>,
    ) -> Self {
        let (stop, consumers_stop) = bounded::<Infallible>(1);
        let (fatal_tx, fatal_rx) = bounded::<ConsumerFatal>(1);
        consumers.start(executor, &consumers_stop, guard, &fatal_tx);

        Self {
            _stop: stop,
            fatal_rx,
            _fatal_tx: fatal_tx,
        }
    }
}

async fn run_server<Exec, E, C>(
    executor: Exec,
    endpoint: E,
    consumers: C,
    addr: SocketAddr,
) -> Result<Shutdown, RuntimeFailure>
where
    Exec: CoreExecutor + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
    C: ConsumerSet,
{
    const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    info!("Skyzen listening on http://{}", local_addr);

    let executor = Arc::new(executor);
    let hyper_executor = HyperExecutor(Arc::clone(&executor));
    let shared_executor: Arc<AnyExecutor> = Arc::new(AnyExecutor::new(Arc::clone(&executor)));

    // Nothing is ever sent through this channel: it exists so that dropping the last sender
    // tells the accept loop every connection task has finished.
    let (connection_guard, connections) = bounded::<Infallible>(1);

    let queue_consumers = ConsumerChannels::start(consumers, executor.as_ref(), &connection_guard);

    let mut incoming = listener.incoming();
    let shutdown_rx = shutdown_signal();
    let shutdown = shutdown_rx.recv().fuse();
    futures_util::pin_mut!(shutdown);
    let mut fatal = None;

    loop {
        futures_util::select! {
            _ = shutdown => {
                info!("Ctrl+C received, stopping accept loop");
                break;
            }
            report = queue_consumers.fatal_rx.recv().fuse() => {
                if let Ok(report) = report {
                    error!(
                        queue = report.queue.as_str(),
                        reason = report.reason,
                        "A declared queue consumer cannot run; stopping"
                    );
                    fatal = Some(report);
                    break;
                }
            }
            connection = incoming.next().fuse() => {
                match connection {
                    Some(Ok(stream)) => {
                        let peer_addr = stream.peer_addr().map_or(None, |peer| {
                            debug!("Accepted connection from {peer}");
                            Some(peer)
                        });
                        let endpoint = endpoint.clone();
                        let shared_executor = shared_executor.clone();
                        let hyper_executor = hyper_executor.clone();
                        let guard = connection_guard.clone();
                        // A second token, handed to the request's `WorkerContext` so post-response
                        // work it spawns keeps shutdown waiting too — the connection's own token
                        // is released as soon as the connection closes.
                        let guard_token = connection_guard.clone();

                        // Spawn the per-connection task *before* sniffing the protocol preface.
                        // Awaiting client bytes inline here would let a single idle client stall
                        // the accept loop for everyone.
                        executor
                            .spawn(async move {
                                // Held for the lifetime of the connection so the shutdown path
                                // can tell when the last one has finished.
                                let _guard = guard;
                                let (stream, is_h2) =
                                    match sniff_protocol_with_timeout(stream, HTTP2_PREFACE).await {
                                        Ok(result) => result,
                                        Err(error) => {
                                            error!("Failed to read connection preface: {error}");
                                            return;
                                        }
                                    };

                                let service = IntoService::new(
                                    endpoint,
                                    shared_executor,
                                    peer_addr,
                                    ShutdownGuard(guard_token),
                                );
                                if is_h2 {
                                    let mut builder = http2::Builder::new(hyper_executor);
                                    // Without a timer hyper silently disables its h2 keep-alive.
                                    builder.timer(AsyncIoTimer);
                                    if let Err(error) = builder
                                        .serve_connection(ConnectionWrapper(stream), service)
                                        .await
                                    {
                                        error!("Hyper h2 connection error: {error}");
                                    }
                                } else {
                                    let mut builder = http1::Builder::new();
                                    // Without a timer hyper silently disables its header read
                                    // timeout, so a slow-loris client would hold the connection.
                                    builder.timer(AsyncIoTimer);
                                    if let Err(error) = builder
                                        .serve_connection(ConnectionWrapper(stream), service)
                                        .with_upgrades()
                                        .await
                                    {
                                        error!("Hyper h1 connection error: {error}");
                                    }
                                }
                            })
                            .detach();
                    }
                    Some(Err(error)) => error!("Accept error: {error}"),
                    None => break,
                }
            }
        }
    }

    // Tell the consumers to stop polling, then release the accept loop's own handle so only live
    // connections and in-flight batches keep the drain channel open.
    drop(queue_consumers);
    drop(connection_guard);
    let severed = drain_connections(&connections, SHUTDOWN_GRACE_PERIOD).await;

    fatal.map_or(Ok(Shutdown { severed }), |report| {
        Err(RuntimeFailure::QueueConsumer(report))
    })
}

/// Hand this process over to the AWS Lambda adapter.
///
/// Without the `lambda` feature there is nothing to hand over to, and the honest answer is to
/// refuse: a Skyzen server that binds a socket inside Lambda is a function that times out on every
/// invocation with nothing in its logs to explain why.
#[cfg(feature = "lambda")]
fn run_on_lambda<Fut, E, C>(
    _options: LaunchOptions,
    factory: impl FnOnce() -> Fut,
) -> Result<core::convert::Infallible, StartupError>
where
    Fut: Future<Output = (E, C)> + Send + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
    C: ConsumerSet,
{
    skyzen_lambda::run(|| async move {
        let (endpoint, consumers) = factory().await;
        (endpoint, LambdaConsumers(consumers))
    })
    .map_err(|error| StartupError::Lambda(error.to_string()))?;

    // `skyzen_lambda::run` returns only when the runtime API stops answering, which is the
    // environment shutting down under us rather than an orderly exit.
    Err(StartupError::Lambda(
        "the Lambda runtime API stopped answering".to_owned(),
    ))
}

#[cfg(not(feature = "lambda"))]
#[allow(clippy::needless_pass_by_value)]
fn run_on_lambda<Fut, E, C>(
    _options: LaunchOptions,
    _factory: impl FnOnce() -> Fut,
) -> Result<core::convert::Infallible, StartupError>
where
    Fut: Future<Output = (E, C)> + Send + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
    C: ConsumerSet,
{
    Err(StartupError::LambdaFeatureMissing)
}

/// The application's consumer set, seen through the Lambda adapter's own trait.
///
/// A local newtype because both the trait and `ConsumerSet`'s implementors would otherwise be
/// foreign to one another; it carries nothing of its own.
#[cfg(feature = "lambda")]
struct LambdaConsumers<C>(C);

#[cfg(feature = "lambda")]
impl<C: ConsumerSet> skyzen_lambda::QueueDispatch for LambdaConsumers<C> {
    const DECLARED: bool = C::DECLARES_HANDLER;

    fn dispatch(
        &self,
        batch: skyzen_services::queue::QueueBatch<Vec<u8>>,
    ) -> impl Future<
        Output = Result<skyzen_services::queue::QueueBatchDisposition, skyzen_services::BoxError>,
    > + Send {
        self.0.dispatch(batch)
    }
}

/// Serve the Azure Functions host on the port it chose, with the queue triggers mounted.
///
/// The host is the only caller, and it is waiting on a port it picked, so that port wins over the
/// default and over `--host`/`--port`. An explicit `--listen` still wins over the host — that is
/// someone running the binary by hand, and silently ignoring what they typed would be worse than
/// serving somewhere the host is not listening.
async fn serve_functions_host<Exec, E, C>(
    executor: Exec,
    endpoint: E,
    consumers: C,
    options: &LaunchOptions,
    port: u16,
) -> Result<Shutdown, RuntimeFailure>
where
    Exec: CoreExecutor + 'static,
    E: Endpoint + ServedRoutes + Clone + Send + Sync + 'static,
    C: ConsumerSet,
{
    let host_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let addr = match options.listen {
        Some(ListenOverride {
            addr,
            explicit: true,
        }) if addr != host_addr => {
            warn!(
                requested = %addr,
                functions_host = %host_addr,
                "--listen overrides the port the Azure Functions host is waiting on; the host \
                 will not reach this process"
            );
            addr
        }
        _ => host_addr,
    };

    info!(
        port,
        triggers = options.azure_queue_triggers.len(),
        "Serving the Azure Functions host as a custom handler"
    );

    let endpoint = azure::mount(endpoint, consumers, options.azure_queue_triggers)
        .map_err(RuntimeFailure::Mount)?;

    // The consumer set moved into the mounted endpoint: under the Functions host the platform
    // pushes every message, so nothing here polls.
    run_server(executor, endpoint, (), addr).await
}

fn server_addr() -> SocketAddr {
    std::env::var("SKYZEN_ADDRESS").map_or_else(
        |_| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        |addr| {
            // Use the provided address by default
            addr.parse()
                .unwrap_or_else(|error| panic!("Invalid SKYZEN_ADDRESS value: {error}"))
        },
    )
}

/// How long a freshly accepted connection may take to send its protocol preface before the
/// connection is dropped. Prevents idle clients from pinning per-connection tasks forever.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run [`sniff_protocol`] with a handshake timeout so a client that connects but never sends
/// any bytes cannot hold a connection task open indefinitely.
async fn sniff_protocol_with_timeout<C>(
    stream: C,
    preface: &[u8],
) -> std::io::Result<(Prefixed<C>, bool)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    use futures_util::future::{select, Either};

    let sniff = sniff_protocol(stream, preface);
    futures_util::pin_mut!(sniff);
    let timer = async_io::Timer::after(HANDSHAKE_TIMEOUT);
    futures_util::pin_mut!(timer);

    match select(sniff, timer).await {
        Either::Left((result, _)) => result,
        Either::Right((_deadline, _)) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out waiting for connection preface",
        )),
    }
}

async fn sniff_protocol<C>(mut stream: C, preface: &[u8]) -> std::io::Result<(Prefixed<C>, bool)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    // A single `read` may return fewer bytes than the full HTTP/2 preface (it can arrive across
    // several TCP segments). Keep reading until we have enough bytes to decide, the bytes so far
    // already diverge from the preface (so it is HTTP/1), or we hit EOF. Whatever we consumed is
    // replayed by `Prefixed`, so no data is lost regardless of the outcome.
    let mut buf = vec![0u8; preface.len()];
    let mut filled = 0;
    while filled < preface.len() {
        if buf[..filled] != preface[..filled] {
            break;
        }
        let n = stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    let is_h2 = buf.starts_with(preface);
    Ok((Prefixed::new(stream, buf), is_h2))
}

#[derive(Debug)]
struct IntoService<E> {
    endpoint: E,
    executor: Arc<AnyExecutor>,
    peer_addr: Option<SocketAddr>,
    /// A clone of the accept loop's drain token, so [`WorkerContext::wait_until`] can register
    /// post-response work with graceful shutdown.
    shutdown_guard: ShutdownGuard,
}

impl<E: Endpoint + Clone> IntoService<E> {
    const fn new(
        endpoint: E,
        executor: Arc<AnyExecutor>,
        peer_addr: Option<SocketAddr>,
        shutdown_guard: ShutdownGuard,
    ) -> Self {
        Self {
            endpoint,
            executor,
            peer_addr,
            shutdown_guard,
        }
    }
}

impl<E: Endpoint + Send + Sync + Clone + 'static> Service<hyper::Request<Incoming>>
    for IntoService<E>
{
    type Response = hyper::Response<
        StreamBody<MapOk<crate::Body, fn(crate::utils::Bytes) -> Frame<crate::utils::Bytes>>>,
    >;
    type Error = BoxHttpError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        let mut endpoint = self.endpoint.clone();
        let executor = self.executor.clone();
        let peer_addr = self.peer_addr;
        let context = WorkerContext::new(self.executor.clone(), self.shutdown_guard.clone());
        let fut = async move {
            let on_upgrade = hyper::upgrade::on(&mut req);
            let method = req.method().clone();
            let path = req.uri().path().to_owned();
            let mut request: crate::Request =
                crate::Request::from(req.map(BodyDataStream::new).map(|body| {
                    crate::Body::from_stream(
                        body.map_err(|error| BodyError::Other(Box::new(error))),
                    )
                }));
            request.extensions_mut().insert(on_upgrade);
            request.extensions_mut().insert(executor);
            request.extensions_mut().insert(context);
            if let Some(peer_addr) = peer_addr {
                request.extensions_mut().insert(PeerAddr(peer_addr));
            }

            // Convert errors to HTTP responses at the runtime level
            let response: crate::Response = match endpoint.respond(&mut request).await {
                Ok(response) => {
                    info!(
                        method = method.as_str(),
                        path = path.as_str(),
                        status = response.status().as_u16(),
                        "request completed"
                    );
                    response
                }
                Err(err) => {
                    // Log and render through the shared helpers so every backend emits the same
                    // fields and applies the same 4xx/5xx redaction policy.
                    skyzen_core::log_endpoint_error(&err, &method, path.as_str());
                    skyzen_core::error_response(&err)
                }
            };

            Ok(response.map(|body| {
                let body: MapOk<
                    crate::Body,
                    fn(crate::utils::Bytes) -> Frame<crate::utils::Bytes>,
                > = body.map_ok(Frame::data);
                StreamBody::new(body)
            }))
        };

        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_cli_overrides, drain_connections, server_addr, sniff_protocol, ListenOverride,
        Platform, Prefixed, StartupError, LAMBDA_RUNTIME_API_ENV,
    };
    use crate::runtime::azure::CUSTOM_HANDLER_PORT_ENV;
    use http_kit::utils::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use serial_test::serial;
    use std::{
        io::{Cursor, Read},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        pin::Pin,
        task::{Context, Poll},
    };

    const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    /// One environment variable, restored to whatever it was when the guard is dropped.
    ///
    /// Platform detection reads the environment, so the tests that exercise it have to write it;
    /// every one of them is `#[serial]`, and this puts the variable back either way.
    struct EnvGuard {
        name: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn capture(name: &'static str) -> Self {
            Self {
                name,
                original: std::env::var(name).ok(),
            }
        }

        fn clear_var(name: &'static str) -> Self {
            let guard = Self::capture(name);
            unsafe {
                std::env::remove_var(name);
            }
            guard
        }

        fn set_var(name: &'static str, value: &str) -> Self {
            let guard = Self::capture(name);
            unsafe {
                std::env::set_var(name, value);
            }
            guard
        }

        fn clear() -> Self {
            Self::clear_var("SKYZEN_ADDRESS")
        }

        fn set(value: &str) -> Self {
            Self::set_var("SKYZEN_ADDRESS", value)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe {
                    std::env::set_var(self.name, value);
                },
                None => unsafe {
                    std::env::remove_var(self.name);
                },
            }
        }
    }

    #[derive(Debug, Default)]
    struct TestStream {
        read: Cursor<Vec<u8>>,
        written: Vec<u8>,
        closed: bool,
    }

    impl TestStream {
        fn new(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                read: Cursor::new(bytes.into()),
                written: Vec::new(),
                closed: false,
            }
        }
    }

    impl AsyncRead for TestStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            Poll::Ready(Read::read(&mut self.read, buf))
        }
    }

    impl AsyncWrite for TestStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            self.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            self.closed = true;
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    #[serial]
    fn server_addr_defaults_to_random_localhost_port() {
        let _guard = EnvGuard::clear();

        assert_eq!(
            server_addr(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        );
    }

    #[test]
    #[serial]
    fn server_addr_uses_environment_override() {
        let _guard = EnvGuard::set("127.0.0.1:4012");

        assert_eq!(
            server_addr(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4012)
        );
    }

    #[test]
    #[serial]
    fn server_addr_fast_fails_for_invalid_environment_value() {
        let _guard = EnvGuard::set("not-an-address");

        let panic = std::panic::catch_unwind(server_addr);

        assert!(panic.is_err());
    }

    #[test]
    #[serial]
    fn a_plain_process_is_detected_as_an_ordinary_server() {
        let _lambda = EnvGuard::clear_var(LAMBDA_RUNTIME_API_ENV);
        let _functions = EnvGuard::clear_var(CUSTOM_HANDLER_PORT_ENV);

        assert_eq!(
            Platform::detect().expect("no host to misread"),
            Platform::Server
        );
    }

    #[test]
    #[serial]
    fn the_lambda_runtime_api_is_detected_before_anything_binds_a_listener() {
        let _lambda = EnvGuard::set_var(LAMBDA_RUNTIME_API_ENV, "127.0.0.1:9001");
        let _functions = EnvGuard::clear_var(CUSTOM_HANDLER_PORT_ENV);

        assert_eq!(
            Platform::detect().expect("a well-formed environment"),
            Platform::Lambda
        );
    }

    #[test]
    #[serial]
    fn lambda_outranks_the_functions_port_when_somehow_both_are_set() {
        // Nothing sets both in practice; this pins the order so a future reader does not have to
        // guess which branch wins.
        let _lambda = EnvGuard::set_var(LAMBDA_RUNTIME_API_ENV, "127.0.0.1:9001");
        let _functions = EnvGuard::set_var(CUSTOM_HANDLER_PORT_ENV, "7071");

        assert_eq!(
            Platform::detect().expect("a well-formed environment"),
            Platform::Lambda
        );
    }

    #[test]
    #[serial]
    fn the_functions_custom_handler_port_is_detected_and_parsed() {
        let _lambda = EnvGuard::clear_var(LAMBDA_RUNTIME_API_ENV);
        let _functions = EnvGuard::set_var(CUSTOM_HANDLER_PORT_ENV, "7071");

        assert_eq!(
            Platform::detect().expect("a well-formed environment"),
            Platform::AzureFunctions { port: 7071 }
        );
    }

    #[test]
    #[serial]
    fn a_functions_port_that_is_not_a_port_stops_the_process_rather_than_guessing() {
        let _lambda = EnvGuard::clear_var(LAMBDA_RUNTIME_API_ENV);
        let _functions = EnvGuard::set_var(CUSTOM_HANDLER_PORT_ENV, "not-a-port");

        let error = Platform::detect().expect_err("there is no sensible port to fall back to");

        assert!(matches!(error, StartupError::CustomHandlerPort { .. }));
        assert!(error.to_string().contains("not-a-port"), "{error}");
    }

    #[test]
    #[cfg(not(feature = "lambda"))]
    fn a_lambda_without_the_feature_names_the_feature_it_needs() {
        let error = StartupError::LambdaFeatureMissing.to_string();

        assert!(error.contains("features = [\"lambda\"]"), "{error}");
        assert!(error.contains(LAMBDA_RUNTIME_API_ENV), "{error}");
    }

    #[test]
    #[serial]
    fn apply_cli_overrides_accepts_listen_aliases_and_split_flags() {
        let _guard = EnvGuard::clear();

        // `--addr`/`--listen` name the whole socket, so they outrank a platform-supplied port.
        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--addr".to_owned(),
                "127.0.0.1:5050".to_owned(),
            ]),
            Some(ListenOverride {
                addr: "127.0.0.1:5050".parse().unwrap(),
                explicit: true,
            })
        );

        // `--host`/`--port` only adjust the default, and lose to one.
        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "-p".to_owned(),
                "6060".to_owned(),
            ]),
            Some(ListenOverride {
                addr: "127.0.0.1:6060".parse().unwrap(),
                explicit: false,
            })
        );
    }

    #[test]
    #[serial]
    fn apply_cli_overrides_returns_none_for_invalid_values() {
        let _guard = EnvGuard::set("127.0.0.1:7000");

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--listen".to_owned(),
                "bad-address".to_owned(),
            ]),
            None
        );

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--host=invalid-host".to_owned(),
                "--port=7001".to_owned(),
            ]),
            None
        );

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--host=127.0.0.1".to_owned(),
                "--port=bad-port".to_owned(),
            ]),
            None
        );

        // A valid override is combined with the env-configured base address.
        assert_eq!(
            apply_cli_overrides(["skyzen".to_owned(), "--port=8080".to_owned()]),
            Some(ListenOverride {
                addr: "127.0.0.1:8080".parse().unwrap(),
                explicit: false,
            })
        );
    }

    #[tokio::test]
    async fn draining_returns_once_every_connection_guard_is_dropped() {
        let (guard, connections) = async_channel::bounded::<std::convert::Infallible>(1);
        let held = guard.clone();
        drop(guard);

        // One connection still running: the deadline wins and reports it as severed.
        assert_eq!(
            drain_connections(&connections, std::time::Duration::from_millis(10)).await,
            1
        );

        // Once it finishes, draining completes with nothing severed.
        drop(held);
        assert_eq!(
            drain_connections(&connections, std::time::Duration::from_secs(30)).await,
            0
        );
    }

    #[tokio::test]
    async fn wait_until_keeps_shutdown_waiting_for_post_response_work() {
        use super::{ShutdownGuard, WorkerContext};
        use executor_core::{smol::SmolGlobal, AnyExecutor};
        use std::sync::Arc;

        let (guard, connections) = async_channel::bounded::<std::convert::Infallible>(1);
        let (release, wait_for_release) = async_channel::bounded::<()>(1);

        let context = WorkerContext::new(
            Arc::new(AnyExecutor::new(SmolGlobal)),
            ShutdownGuard(guard.clone()),
        );
        context
            .wait_until(async move {
                let _ = wait_for_release.recv().await;
            })
            .expect("the native context always accepts post-response work");

        // The request is over: the connection's own token and the context that spawned the work
        // are both gone, but the spawned task holds a token of its own.
        drop(context);
        drop(guard);
        assert_eq!(
            drain_connections(&connections, std::time::Duration::from_millis(50)).await,
            1
        );

        // Letting the post-response work finish releases the last token, and the drain completes.
        release.send(()).await.expect("the task should be waiting");
        assert_eq!(
            drain_connections(&connections, std::time::Duration::from_secs(30)).await,
            0
        );
    }

    #[tokio::test]
    async fn sniff_protocol_detects_http2_preface_and_replays_buffered_bytes() {
        let payload = [HTTP2_PREFACE, b"rest"].concat();
        let (mut stream, is_h2) = sniff_protocol(TestStream::new(payload.clone()), HTTP2_PREFACE)
            .await
            .unwrap();

        assert!(is_h2);

        let mut read = Vec::new();
        stream.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, payload);
    }

    #[tokio::test]
    async fn sniff_protocol_distinguishes_http1_and_preserves_writes() {
        let payload = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let (mut stream, is_h2) = sniff_protocol(TestStream::new(payload.clone()), HTTP2_PREFACE)
            .await
            .unwrap();

        assert!(!is_h2);

        let mut read = Vec::new();
        stream.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, payload);

        stream.write_all(b"pong").await.unwrap();
        stream.flush().await.unwrap();
        stream.close().await.unwrap();
        assert_eq!(stream.inner.written, b"pong".to_vec());
        assert!(stream.inner.closed);
    }

    #[tokio::test]
    async fn prefixed_reads_buffer_before_inner_stream() {
        let mut stream = Prefixed::new(TestStream::new(b"tail".to_vec()), b"head".to_vec());

        let mut read = Vec::new();
        stream.read_to_end(&mut read).await.unwrap();

        assert_eq!(read, b"headtail".to_vec());
    }

    // A stream that yields its bytes in caller-chosen chunks, to exercise `sniff_protocol`'s
    // short-read handling (the HTTP/2 preface arriving across multiple reads).
    struct ChunkedStream {
        chunks: std::collections::VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }

    impl ChunkedStream {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: std::collections::VecDeque::from(chunks),
                written: Vec::new(),
            }
        }
    }

    impl AsyncRead for ChunkedStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
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
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            this.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
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

    #[tokio::test]
    async fn sniff_detects_split_http2_preface() {
        let chunks = vec![
            HTTP2_PREFACE[..5].to_vec(),
            HTTP2_PREFACE[5..12].to_vec(),
            HTTP2_PREFACE[12..].to_vec(),
        ];
        let stream = ChunkedStream::new(chunks);

        let (_prefixed, is_h2) = sniff_protocol(stream, HTTP2_PREFACE).await.unwrap();
        assert!(is_h2);
    }

    #[tokio::test]
    async fn sniff_preserves_bytes_on_split_mismatch() {
        let payload = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let chunks = vec![
            payload[..3].to_vec(),
            payload[3..10].to_vec(),
            payload[10..].to_vec(),
        ];
        let stream = ChunkedStream::new(chunks);

        let (prefixed, is_h2) = sniff_protocol(stream, HTTP2_PREFACE).await.unwrap();
        assert!(!is_h2);

        let restored = read_all(prefixed).await;
        assert_eq!(restored, payload);
    }
}
