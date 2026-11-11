use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroUsize,
    sync::Arc,
};

use crate::{
    routing::ServedRoutes,
    runtime::{
        azure,
        consumer::{ConsumerFatal, ConsumerSet},
    },
    Endpoint,
};
use async_channel::{bounded, Receiver, Sender};
use core::convert::Infallible;
use executor_core::{compio::Runtime, smol::SmolGlobal, try_init_global_executor, AnyExecutor};
use futures_util::future::FutureExt;
use skyzen_core::EndpointFactory;
use tracing::{debug, error, info, warn};
use tracing_log::log::LevelFilter as LogLevelFilter;
use tracing_subscriber::EnvFilter;

mod busy_poll;
mod cores;
mod handoff;
mod http3;
mod io_settings;
pub mod steering;
mod tls;

use busy_poll::BusyPollError;
pub use busy_poll::{BusyPoll, BusyPollTimeout};
pub use io_settings::{
    apply_io_overrides, IoOptions, IoSettings, IoSettingsNotConfigured, RecvRing,
};

use http3::{Http3, Http3Error};
use steering::SteeringError;
pub use steering::{apply_steering_override, Steering};
use tls::{ServerTls, TlsError};

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

/// An address the native server can listen on.
///
/// One grammar everywhere — `--listen`, `--addr` and `SKYZEN_ADDRESS` all parse this:
/// a `host:port` socket address is TCP, and a `unix:`-prefixed path is a filesystem unix
/// domain socket. `unix:///abs/path` is accepted too; the empty authority is ignored.
///
/// A unix socket peer has no `SocketAddr`, so `PeerAddr` and `ClientIp` do not resolve
/// on connections accepted that way — a fronting proxy reports its client through
/// `Forwarded`/`X-Forwarded-For` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddr {
    /// A TCP `host:port` address — the only kind every platform supports.
    Tcp(SocketAddr),
    /// A filesystem unix socket path, e.g. `unix:/run/app.sock`.
    ///
    /// The socket file belongs to the process while it runs: a path that is already a socket
    /// is probed before binding (a live server keeps its socket; a dead one's leftover is
    /// removed and rebound), and the file is unlinked when the server stops.
    #[cfg(unix)]
    Unix(std::path::PathBuf),
}

/// Why a `--listen`/`SKYZEN_ADDRESS` value is not a [`ListenAddr`].
#[derive(Debug)]
pub enum ListenAddrError {
    /// Neither a TCP `host:port` nor a `unix:` path.
    NotAnAddress(std::net::AddrParseError),
    /// `unix:` named no path.
    EmptyUnixPath,
    /// A `unix:` path on a platform that has no unix domain sockets.
    #[cfg(not(unix))]
    UnixUnsupported,
}

impl std::fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "{addr}"),
            #[cfg(unix)]
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

impl std::fmt::Display for ListenAddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnAddress(error) => {
                write!(
                    f,
                    "not a `host:port` socket address or a `unix:` path: {error}"
                )
            }
            Self::EmptyUnixPath => write!(f, "`unix:` names no socket path"),
            #[cfg(not(unix))]
            Self::UnixUnsupported => {
                write!(f, "unix domain sockets do not exist on this platform")
            }
        }
    }
}

impl std::error::Error for ListenAddrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotAnAddress(error) => Some(error),
            Self::EmptyUnixPath => None,
            #[cfg(not(unix))]
            Self::UnixUnsupported => None,
        }
    }
}

/// The prefix a filesystem unix socket path carries in `--listen`/`SKYZEN_ADDRESS`.
const UNIX_SCHEME: &str = "unix:";

impl std::str::FromStr for ListenAddr {
    type Err = ListenAddrError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(path) = value.strip_prefix(UNIX_SCHEME) else {
            return value
                .parse()
                .map(Self::Tcp)
                .map_err(ListenAddrError::NotAnAddress);
        };
        Self::unix(path)
    }
}

impl ListenAddr {
    /// Interpret what follows `unix:` as a filesystem socket path.
    #[cfg(unix)]
    fn unix(path: &str) -> Result<Self, ListenAddrError> {
        // `unix:///abs/path` carries an empty authority — drop it so `unix:rel`,
        // `unix:/abs` and `unix:///abs` all mean what they look like.
        let path = path.strip_prefix("//").unwrap_or(path);
        if path.is_empty() {
            return Err(ListenAddrError::EmptyUnixPath);
        }
        Ok(Self::Unix(std::path::PathBuf::from(path)))
    }

    /// Unix domain sockets do not exist on this platform, so a `unix:` path can only be a
    /// mistake — say so rather than misreading it as a TCP address.
    #[cfg(not(unix))]
    const fn unix(_path: &str) -> Result<Self, ListenAddrError> {
        Err(ListenAddrError::UnixUnsupported)
    }
}

/// A listener address a command-line flag asked for.
///
/// Which flag it came from matters: a serverless host that dictates the port (Azure Functions
/// does) outranks the defaults and outranks `--host`/`--port`, which only adjust them — but not an
/// explicit `--listen`, where the operator named the whole socket and meant it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenOverride {
    /// The address to bind.
    pub addr: ListenAddr,
    /// Whether `--listen`/`--addr` named this socket outright.
    pub explicit: bool,
}

/// Parse CLI overrides such as `--addr`/`--port` and return the resulting listen address.
///
/// `--listen`/`--addr` name the whole socket outright — a TCP `host:port`, or `unix:/path`
/// for a filesystem unix socket on unix platforms. `--host`/`--port` only adjust the TCP
/// address [`server_addr`] produced, so a unix `SKYZEN_ADDRESS` cannot be mutated by them;
/// they are ignored with a warning rather than silently switching transports.
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
        return match addr.parse::<ListenAddr>() {
            Ok(addr) => {
                info!("Configured listener address via CLI: {addr}");
                Some(ListenOverride {
                    addr,
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
            Ok(ip) => match &mut candidate {
                ListenAddr::Tcp(addr) => addr.set_ip(ip),
                #[cfg(unix)]
                ListenAddr::Unix(_) => {
                    warn!("Ignoring --host `{host}`: SKYZEN_ADDRESS names a unix socket");
                }
            },
            Err(error) => {
                warn!("Ignoring invalid --host `{host}`: {error}");
                return None;
            }
        }
    }
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(value) => match &mut candidate {
                ListenAddr::Tcp(addr) => addr.set_port(value),
                #[cfg(unix)]
                ListenAddr::Unix(_) => {
                    warn!("Ignoring --port `{port}`: SKYZEN_ADDRESS names a unix socket");
                }
            },
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

/// A `--tls-cert`/`--tls-key` flag that reached us at all promised TLS; a
/// missing value is not ignorable — swallowing it would serve cleartext under
/// a flag that promised encryption, which is worse than any panic.
fn flag_value(flag: &str, value: Option<String>) -> String {
    match value {
        Some(value) if !value.is_empty() && !value.starts_with('-') => value,
        _ => panic!("{flag} expects a PEM path after it"),
    }
}

/// The PEM files the native server terminates TLS with.
///
/// One grammar everywhere — `--tls-cert`/`--tls-key` and the `SKYZEN_TLS_CERT`/
/// `SKYZEN_TLS_KEY` environment variables name the same pair of files, and each
/// flag wins over its variable independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsFiles {
    /// PEM file holding the certificate chain, leaf certificate first.
    pub cert: std::path::PathBuf,
    /// PEM file holding the private key matching the leaf certificate.
    pub key: std::path::PathBuf,
}

/// Parse `--tls-cert`/`--tls-key`, falling back per file to `SKYZEN_TLS_CERT`/
/// `SKYZEN_TLS_KEY`.
///
/// Returns `None` when TLS is not configured at all; the server then serves
/// cleartext, as it always has.
///
/// # Panics
///
/// A pair that is only half supplied — or that names an empty path — kills the
/// process at startup, for the same reason an invalid `SKYZEN_ADDRESS` does:
/// half a TLS setup is a configuration error, not a hint to serve plaintext
/// under flags that promised encryption.
#[must_use]
pub fn apply_tls_overrides(args: impl IntoIterator<Item = String>) -> Option<TlsFiles> {
    const CERT_ENV: &str = "SKYZEN_TLS_CERT";
    const KEY_ENV: &str = "SKYZEN_TLS_KEY";

    let mut args = args.into_iter();
    let _ = args.next(); // binary name
    let mut cert_flag = None;
    let mut key_flag = None;
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--tls-cert=") {
            cert_flag = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--tls-key=") {
            key_flag = Some(value.to_owned());
        } else {
            match arg.as_str() {
                "--tls-cert" => cert_flag = Some(flag_value("--tls-cert", args.next())),
                "--tls-key" => key_flag = Some(flag_value("--tls-key", args.next())),
                _ => {}
            }
        }
    }

    // `var_os`, not `var`: a non-UTF-8 path is still a configured TLS file, and
    // `var`'s `.ok()` would read it as absent and boot the server plaintext —
    // the worst outcome under a configuration that promised encryption.
    let cert = cert_flag
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os(CERT_ENV).map(std::path::PathBuf::from));
    let key = key_flag
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os(KEY_ENV).map(std::path::PathBuf::from));
    match (cert, key) {
        (None, None) => None,
        (Some(cert), Some(key)) => {
            assert!(
                !cert.as_os_str().is_empty(),
                "an empty path is not a TLS certificate"
            );
            assert!(
                !key.as_os_str().is_empty(),
                "an empty path is not a TLS key"
            );
            info!(
                "Configured TLS certificate {} with key {}",
                cert.display(),
                key.display()
            );
            Some(TlsFiles { cert, key })
        }
        (Some(_), None) => panic!(
            "a TLS certificate was configured without a key; pass --tls-key or set {KEY_ENV}"
        ),
        (None, Some(_)) => panic!(
            "a TLS key was configured without a certificate; pass --tls-cert or set {CERT_ENV}"
        ),
    }
}

/// The server that answers cleartext HTTP on the serving threads.
///
/// TLS is not a choice: `skyzen-h1` serves cleartext only, so a TLS listener is always hyper's,
/// with ALPN choosing HTTP/1.1 or HTTP/2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HttpServer {
    /// Skyzen's own HTTP/1.1 server on owned-buffer I/O (`skyzen-h1`), the default.
    #[default]
    H1,
    /// Hyper: HTTP/1.1 and prior-knowledge HTTP/2 (h2c), over the compio stream's copying
    /// poll-style bridge. Without the per-thread connection limit the default server applies.
    Hyper,
}

impl std::str::FromStr for HttpServer {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "h1" => Ok(Self::H1),
            "hyper" => Ok(Self::Hyper),
            other => Err(format!("`{other}` is not a server; use `h1` or `hyper`")),
        }
    }
}

/// Parse `--server`, falling back to `SKYZEN_SERVER`, and to the default [`HttpServer::H1`].
///
/// # Panics
///
/// A value that names no server kills the process at startup: silently serving the other
/// protocol set than the one asked for is worse than not starting.
#[must_use]
pub fn apply_server_override(args: impl IntoIterator<Item = String>) -> HttpServer {
    const ENV: &str = "SKYZEN_SERVER";

    let mut args = args.into_iter();
    let _ = args.next(); // binary name
    let mut flag = None;
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--server=") {
            flag = Some(value.to_owned());
        } else if arg == "--server" {
            flag = Some(
                args.next()
                    .unwrap_or_else(|| panic!("--server expects `h1` or `hyper` after it")),
            );
        }
    }

    let value = flag.or_else(|| std::env::var(ENV).ok());
    let Some(value) = value else {
        return HttpServer::default();
    };
    let server = value
        .parse()
        .unwrap_or_else(|error| panic!("invalid server: {error}"));
    info!("Configured HTTP server: {value}");
    server
}

/// Whether `--http3` or `SKYZEN_HTTP3` asks for HTTP/3, which the server then refuses to start
/// without.
///
/// The flag stands alone; the variable is `true`/`1` or `false`/`0`, and the flag wins.
///
/// # Panics
///
/// A variable that is neither kills the process at startup, like every other malformed setting.
#[must_use]
pub fn apply_http3_override(args: impl IntoIterator<Item = String>) -> bool {
    const ENV: &str = "SKYZEN_HTTP3";

    if args.into_iter().skip(1).any(|arg| arg == "--http3") {
        info!("HTTP/3 requested by --http3");
        return true;
    }
    let Some(value) = std::env::var_os(ENV) else {
        return false;
    };
    match value.to_str() {
        Some("true" | "1") => {
            info!("HTTP/3 requested by {ENV}");
            true
        }
        Some("false" | "0") => false,
        _ => panic!(
            "{ENV} must be `true`, `1`, `false` or `0`, not {}",
            value.to_string_lossy()
        ),
    }
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

/// How long outstanding work is given to finish after the accept loops stop.
///
/// A request still streaming a response — or a queue consumer still settling the batch it is
/// holding — when the deadline elapses is severed; the runtime says so in its final log line
/// rather than claiming a graceful shutdown.
pub const SHUTDOWN_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(30);

/// What the serving threads had left to clean up when they stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shutdown {
    /// Open connections, requests, post-response tasks and consumer batches still in flight when
    /// the grace period elapsed, and therefore cut off.
    pub severed: usize,
}

/// Why the runtime stopped before it could shut down on its own terms.
#[derive(Debug)]
enum RuntimeFailure {
    /// The listener could not be bound, or a serving thread could not start.
    Listener(std::io::Error),
    /// A declared queue consumer cannot run at all against the backend it was pointed at.
    QueueConsumer(ConsumerFatal),
    /// The Azure Functions triggers cannot be mounted over this application.
    Mount(azure::MountError),
    /// A serving thread died instead of reporting that it was accepting.
    ThreadExited,
    /// TLS was configured but cannot be provided on this listener or this build.
    Tls(TlsError),
    /// HTTP/3 was requested and cannot be served, or its endpoint cannot start.
    Http3(Http3Error),
    /// Connection steering was forced on and cannot be had, or a thread cannot be pinned.
    Steering(SteeringError),
    /// Busy polling was turned on and cannot be had.
    BusyPoll(BusyPollError),
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
            Self::ThreadExited => write!(f, "a serving thread exited before it was accepting"),
            Self::Tls(error) => write!(f, "{error}"),
            Self::Http3(error) => write!(f, "{error}"),
            Self::Steering(error) => write!(f, "{error}"),
            Self::BusyPoll(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RuntimeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Listener(error) => Some(error),
            Self::Mount(error) => Some(error),
            Self::Tls(error) => Some(error),
            Self::Http3(error) => Some(error),
            Self::Steering(error) => Some(error),
            Self::BusyPoll(error) => Some(error),
            Self::QueueConsumer(_) | Self::ThreadExited => None,
        }
    }
}

/// Everything `#[skyzen::main]` settles before the runtime starts.
///
/// A struct rather than a widening argument list: what the runtime needs to know grows with every
/// platform it learns to serve, and each of those is a compile-time fact read out of `Skyzen.toml`.
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// The address a command-line flag asked for, if any.
    pub listen: Option<ListenOverride>,
    /// The TLS certificate/key pair the server terminates TLS with, from
    /// `--tls-cert`/`--tls-key` or `SKYZEN_TLS_CERT`/`SKYZEN_TLS_KEY`.
    /// `None` serves cleartext, as the runtime always has.
    pub tls: Option<TlsFiles>,
    /// Whether `--http3` or `SKYZEN_HTTP3` insisted on HTTP/3.
    ///
    /// HTTP/3 is served on UDP beside the TLS listener whenever TLS is configured and the `http3`
    /// feature is compiled in; asking for it makes every case where that cannot happen a refusal
    /// to start instead of a TLS-only server.
    pub http3: bool,
    /// Whether connections are steered to the thread on the CPU that received them, from
    /// `--steering` or `SKYZEN_STEERING`.
    pub steering: Steering,
    /// The serving threads' receive ring, zero-copy threshold and file-table size, from their
    /// flags and variables (see [`IoSettings`]).
    pub io: IoOptions,
    /// The server that answers cleartext connections, from `--server` or `SKYZEN_SERVER`.
    pub server: HttpServer,
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

pub use skyzen_core::OPENAPI_DUMP_ENV;

/// Print the application's `OpenAPI` document and report that the process should now exit, if
/// [`OPENAPI_DUMP_ENV`] asked for it.
///
/// Returns `false` — having touched nothing, not even `factory` — when the variable is unset, so
/// the ordinary startup path pays a single `getenv` for this.
///
/// `#[skyzen::main]` calls this *before* wiring services into the router, which is the whole point:
/// a document describes the shape of an API, not a connection to anything, so producing one must
/// not require credentials, a reachable database or a provisioned queue. `skyzen openapi` relies on
/// that — it runs the binary in a project whose backends may not exist yet.
///
/// # Panics
///
/// Never returns on a failure: an unwritable path or a build with the `openapi` feature off exits
/// non-zero, because a caller asked for a document and a silent empty one is worse than no answer.
pub fn dump_openapi_if_requested<Fut, E>(factory: impl FnOnce() -> Fut) -> bool
where
    Fut: Future<Output = E>,
    E: ServedRoutes,
{
    let Some(target) = std::env::var_os(OPENAPI_DUMP_ENV) else {
        return false;
    };

    let document = Runtime::new()
        .expect("a compio runtime to build the router on")
        .block_on(factory())
        .openapi_document();
    if !document.is_enabled() {
        error!(
            "{OPENAPI_DUMP_ENV} was set, but this binary was built without skyzen's `openapi` \
             feature, so it has no document to print"
        );
        std::process::exit(1);
    }

    let json = match serde_json::to_string_pretty(&document.to_utoipa_spec()) {
        Ok(json) => json,
        Err(error) => {
            error!("Failed to serialize the OpenAPI document: {error}");
            std::process::exit(1);
        }
    };

    let path = std::path::Path::new(&target);
    if path == std::path::Path::new("-") {
        // The one place a Skyzen binary writes to stdout: the caller asked for the document *as*
        // this process's output, so a logger would put it in the wrong stream.
        use std::io::Write as _;
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = writeln!(stdout, "{json}").and_then(|()| stdout.flush()) {
            error!("Failed to write the OpenAPI document to stdout: {error}");
            std::process::exit(1);
        }
    } else if let Err(error) = std::fs::write(path, &json) {
        error!(
            "Failed to write the OpenAPI document to {}: {error}",
            path.display()
        );
        std::process::exit(1);
    }

    true
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

/// Where the HTTP service clients `#[skyzen::main]` builds — the AWS and Azure backends, on
/// zenwave — keep their connections, as the platform decides.
///
/// On a thread-per-core server, and under the Azure Functions host that is one, every thread runs
/// a compio runtime, and each rebinds its clients to a connection pool of its own: a service
/// request made on a serving thread is sent on a connection that thread's runtime drives. The
/// coordinating thread does the same for the clients the queue consumers poll with, and owns the
/// pools the AWS credentials are fetched on: setup, which runs there, loads the one AWS
/// configuration every AWS client is built from on [`driver`](Self::driver), so a credentials
/// refresh a serving thread's call starts goes out on the coordinating thread's connections,
/// which outlive the drain. Under AWS Lambda the one thread runs the adapter's tokio runtime, so
/// the clients and the credentials keep the pools zenwave runs on its own driver thread.
///
/// The driver a client pools on is the one the serving threads' own sockets run on, carrying the
/// operator's [`IoSettings`]: a ring's registered file table belongs to every stream of that
/// ring, and a client's large writes are sent zero-copy from the same threshold as a response's.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct ServicePools {
    driver: Option<http_kit_io::compio::Compio>,
}

impl ServicePools {
    /// The calling thread's compio driver, to pool on, where the platform's threads run compio;
    /// `None` where they do not, and a client keeps zenwave's own driver thread.
    #[must_use]
    pub fn driver(&self) -> Option<http_kit_io::compio::Compio> {
        self.driver.clone()
    }

    /// `service` as the calling thread uses it: `rebind` hands it the thread's
    /// [`driver`](Self::driver) to pool on where there is one; otherwise it is returned as built.
    ///
    /// # Panics
    ///
    /// `rebind` panics when it runs on a thread that is not running a compio runtime, which on a
    /// platform that pools per thread is a thread Skyzen did not start.
    pub fn on_this_thread<S>(
        &self,
        service: S,
        rebind: impl FnOnce(S, http_kit_io::compio::Compio) -> S,
    ) -> S {
        match self.driver() {
            Some(io) => rebind(service, io),
            None => service,
        }
    }
}

/// Serve the application on one compio runtime per core, and run the declared queue consumers
/// beside them.
///
/// `setup` builds the application's services once and returns an [`EndpointFactory`] that
/// captures them together with the [`ConsumerSet`] — `()` for an application that declares no
/// `[[native.queue_consumer]]` — so both are built from the same service instances rather than
/// from two independent connections to one backend. Every serving thread calls the factory on
/// itself, so each has a router of its own and the router and everything it holds may be `!Send`.
/// `setup` is handed the platform's [`ServicePools`], which the factory carries to each thread so
/// the thread rebinds its HTTP service clients to connection pools of its own.
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
/// - Anything else binds the address the flags, `SKYZEN_ADDRESS` or the default asked for —
///   a TCP `host:port`, or `unix:/path` for a filesystem unix socket on unix platforms.
///
/// Under either serverless host the declared `[[native.queue_consumer]]` polling loops do *not*
/// run: the platform owns delivery there, and a loop polling a queue inside a function that scales
/// to zero would take messages nothing is waiting to process.
///
/// `on_shutdown` does not run on the Lambda path either. Lambda freezes and later discards an
/// execution environment without telling the process, so there is no moment at which a cleanup
/// hook could be called and be believed.
///
/// # Threads
///
/// One thread per unit of [`std::thread::available_parallelism`] serves, each on a compio
/// runtime of its own with its own listener (`SO_REUSEPORT` where the platform has it), its own
/// router, and its own connections: a connection never leaves the thread that accepted it. HTTP/1.1
/// is served by `skyzen-h1`. The coordinating thread, which runs `setup`, the consumers and the
/// shutdown, does not serve.
///
/// After `Ctrl+C` every listener stops accepting and the consumers stop initiating receives;
/// outstanding connections, post-response tasks and in-flight batches are awaited for up to
/// [`SHUTDOWN_GRACE_PERIOD`], and only then does `on_shutdown` run — so a cleanup hook observes a
/// server with no requests and no batches still in flight. A serving thread keeps its runtime
/// running until then, so a post-response task still reaches the connection pools of the thread
/// whose request spawned it.
///
/// A consumer that can never receive at all, and a listener that cannot be bound, both end the
/// process with a non-zero status once `on_shutdown` has run: a misconfigured deployment should
/// be restarted by its supervisor, not left running half-alive.
///
/// # Panics
///
/// Panics if the coordinating thread's runtime cannot be created.
pub fn launch<Setup, F, C, Hook, HookFut>(
    options: LaunchOptions,
    setup: impl FnOnce(ServicePools) -> Setup,
    on_shutdown: Hook,
) where
    Setup: Future<Output = (F, C)>,
    F: EndpointFactory,
    F::Endpoint: ServedRoutes,
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

    if platform != Platform::Server {
        if let Some(refusal) = socket_settings_refusal(&options) {
            error!("Skyzen cannot start: {refusal}");
            std::process::exit(1);
        }
    }

    if platform == Platform::Lambda {
        // Lambda's runtime is Tokio's and the adapter owns it, so this path never touches the
        // executor, the listener or the shutdown drain below.
        // `run_on_lambda` never returns `Ok`: it either hands over for the life of the process
        // or explains why it could not.
        let Err(error) = run_on_lambda(options, setup);
        error!("Skyzen cannot start: {error}");
        std::process::exit(1);
    }

    // The global executor runs `Send` background work: queue consumers and post-response tasks.
    if try_init_global_executor(SmolGlobal).is_err() {
        debug!("Global executor already initialized; reusing existing instance");
    }

    // This thread coordinates: it builds the services, starts the serving threads and waits for
    // the signal to stop them.
    let runtime = Runtime::new().expect("a compio runtime for the coordinating thread");
    let failed = runtime.block_on(async move {
        tracing::info!("Skyzen application starting up");

        // Every thread this path runs services on — this one and each serving thread — runs a
        // compio runtime its clients can pool on, with the driver the serving threads' sockets
        // run on.
        let driver = options.io.driver();
        let (factory, consumers) = setup(ServicePools {
            driver: Some(driver.clone()),
        })
        .await;
        let outcome = match platform {
            Platform::Lambda => unreachable!("handed over before the runtime was built"),
            Platform::Server => {
                let addr = options
                    .listen
                    .as_ref()
                    .map_or_else(server_addr, |listen| listen.addr.clone());
                let secured = tls::build(options.tls.as_ref(), &addr).and_then(|tls| {
                    let http3 = http3::build(options.http3, tls.as_ref(), &addr)?;
                    Ok((tls, http3))
                });
                match secured {
                    Ok((tls, http3)) => {
                        serve_cores(
                            factory,
                            Ok,
                            consumers,
                            addr,
                            ListenerSetup {
                                server: options.server,
                                tls,
                                http3,
                                steering: options.steering,
                                io: options.io,
                                driver,
                            },
                        )
                        .await
                    }
                    Err(error) => Err(error),
                }
            }
            Platform::AzureFunctions { port } => {
                serve_functions_host(factory, consumers, &options, driver, port).await
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

/// Why `options` cannot start on a serverless host, which owns the sockets: a setting that only
/// the path binding its own sockets applies would silently go dead there, so it is refused.
fn socket_settings_refusal(options: &LaunchOptions) -> Option<String> {
    // Both serverless hosts terminate TLS upstream, so a configured pair there can only be a
    // leftover.
    if options.tls.is_some() {
        return Some(
            "TLS is configured (--tls-cert/--tls-key or SKYZEN_TLS_CERT/SKYZEN_TLS_KEY), but \
             this platform terminates TLS upstream; unset them or run where they apply"
                .to_owned(),
        );
    }
    // The same holds for HTTP/3: a serverless host owns the UDP side, if it has one.
    if options.http3 {
        return Some(http3::not_a_server().to_string());
    }
    if options.steering == Steering::On {
        return Some(SteeringError::NotServer.to_string());
    }
    if options.io.busy_poll.is_some() {
        return Some(BusyPollError::NotServer.to_string());
    }
    None
}

/// Wait for every holder of the drain channel's sender to finish, or for the grace period to
/// elapse.
///
/// The serving threads, the requests in flight on them, post-response tasks and queue-consumer
/// batches each hold a clone of the sender, so `recv` resolves — with a closed-channel error,
/// since nothing is ever sent — exactly when the last one is dropped. The senders still alive at
/// the deadline are the work being cut off.
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

/// The channels tying the coordinator to the queue consumers it started.
///
/// Dropping the whole thing is what tells every consumer slot to stop initiating receives, so the
/// shutdown path cannot forget one of the two halves.
#[derive(Debug)]
struct ConsumerChannels {
    /// Watched by every consumer slot; closing it stops them.
    _stop: Sender<Infallible>,
    /// Where a consumer that can never receive reports itself.
    fatal_rx: Receiver<ConsumerFatal>,
    /// The coordinator's own sender, kept alive so [`Self::fatal_rx`] stays open: a closed
    /// channel would resolve immediately and read as a failure for an application that declares
    /// no consumers at all.
    _fatal_tx: Sender<ConsumerFatal>,
}

impl ConsumerChannels {
    /// Start `consumers` on `executor`, each slot holding a clone of the drain `guard`.
    fn start<Exec: executor_core::Executor + 'static, C: ConsumerSet>(
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

/// The first serving thread to fail instead of reporting that it is accepting.
async fn first_failure(started: &cores::Cores) -> Option<RuntimeFailure> {
    for _ in 0..started.threads {
        match started.ready.recv().await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Some(error),
            Err(_) => return Some(RuntimeFailure::ThreadExited),
        }
    }
    None
}

/// How the listeners of [`serve_cores`] are served beyond the address they bind.
struct ListenerSetup {
    /// The server that answers cleartext connections.
    server: HttpServer,
    /// The acceptor, when the listener terminates TLS.
    tls: Option<ServerTls>,
    /// HTTP/3, when it is served beside the TLS listener.
    http3: Option<Http3>,
    /// Whether connections are steered to the CPU that received them.
    steering: Steering,
    /// The operator's I/O settings, of which the receive ring is sized here.
    io: IoOptions,
    /// The driver every serving thread's sockets run on, the one the service clients pool on.
    driver: http_kit_io::compio::Compio,
}

/// Bind `addr`, start one serving thread per core, and run until `Ctrl+C`, a fatal consumer
/// report, or the serving threads' own end.
///
/// `wrap` turns a thread's freshly built router into the endpoint that thread serves — the
/// identity for an ordinary server, the queue-trigger mount under the Azure Functions host. It
/// runs on the serving thread, so what it returns need not be `Send`.
async fn serve_cores<F, A, W, C>(
    factory: F,
    wrap: W,
    consumers: C,
    addr: ListenAddr,
    setup: ListenerSetup,
) -> Result<Shutdown, RuntimeFailure>
where
    F: EndpointFactory,
    A: Endpoint + Clone + 'static,
    W: Fn(F::Endpoint) -> Result<A, RuntimeFailure> + Send + Sync + 'static,
    C: ConsumerSet,
{
    let ListenerSetup {
        server,
        tls,
        http3,
        steering,
        io,
        driver,
    } = setup;
    let threads = std::thread::available_parallelism()?;
    let pins =
        steering::decide(steering, threads.get(), &addr).map_err(RuntimeFailure::Steering)?;
    busy_poll::check(io.busy_poll.as_ref(), &addr).map_err(RuntimeFailure::BusyPoll)?;
    let mut bound = cores::bind(&addr, threads, pins, io.busy_poll.as_ref())?;
    // QUIC takes the port TLS has on TCP, so a client that follows `Alt-Svc` stays on the host
    // and port it came to. Bound here, with the TCP sockets, so a port that cannot be had fails
    // the start before any thread exists.
    let http3_port = match (
        &http3,
        bound.sockets.first().and_then(cores::Socket::tcp_addr),
    ) {
        (Some(http3), Some(local)) => {
            let local = local?;
            bound.udp = http3.bind(local, threads, io.busy_poll.as_ref())?;
            Some(local.port())
        }
        _ => None,
    };
    let scheme = if tls.is_some() { "https" } else { "http" };
    let http3_note = http3_port.map_or_else(String::new, |port| format!(" (HTTP/3 on udp/{port})"));
    let listening = match bound.sockets.first().and_then(cores::Socket::tcp_addr) {
        Some(local) => format!("{scheme}://{}", local?),
        None => addr.to_string(),
    };

    // A drain token for everything that must finish before the process may stop, and the
    // broadcast that tells the threads to begin, and the one that lets them end once the drain is
    // over. Nothing is sent through any of them.
    let (drain, connections) = bounded::<Infallible>(1);
    let (stop, stopping) = bounded::<Infallible>(1);
    let (release, released) = bounded::<Infallible>(1);

    let queue_consumers = ConsumerChannels::start(consumers, &SmolGlobal, &drain);

    let max_connections =
        NonZeroUsize::new(MAX_CONNECTIONS.div_ceil(threads.get())).unwrap_or(NonZeroUsize::MIN);
    let plan = cores::Plan {
        factory,
        wrap,
        executor: Arc::new(AnyExecutor::new(SmolGlobal)),
        max_connections,
        io: IoSettings::new(driver, io.recv_ring(max_connections), io.busy_poll),
        server,
        tls,
        http3: http3.zip(http3_port).map(|(http3, port)| cores::Http3Plan {
            http3,
            alt_svc: skyzen_hyper::AltSvc::h3(port),
        }),
    };
    let started = cores::start(plan, bound, &stopping, &drain, &released)?;

    let mut failure = first_failure(&started).await;

    if failure.is_none() {
        info!(
            threads = started.threads,
            "Skyzen listening on {listening}{http3_note}"
        );

        let shutdown_rx = shutdown_signal();
        let shutdown = shutdown_rx.recv().fuse();
        let exited = started.exited.recv().fuse();
        futures_util::pin_mut!(shutdown, exited);
        loop {
            futures_util::select! {
                _ = shutdown => {
                    info!("Ctrl+C received, stopping the accept loops");
                    break;
                }
                report = queue_consumers.fatal_rx.recv().fuse() => {
                    if let Ok(report) = report {
                        error!(
                            queue = report.queue.as_str(),
                            reason = report.reason,
                            "A declared queue consumer cannot run; stopping"
                        );
                        failure = Some(RuntimeFailure::QueueConsumer(report));
                        break;
                    }
                }
                _ = exited => {
                    failure = Some(RuntimeFailure::ThreadExited);
                    break;
                }
            }
        }
    }

    // Tell the consumers to stop polling and the threads to wind down, then release the
    // coordinator's own handle so only live threads, requests and batches keep the drain channel
    // open.
    drop(queue_consumers);
    drop(stop);
    drop(drain);
    let severed = drain_connections(&connections, SHUTDOWN_GRACE_PERIOD).await;
    // Nothing that could still send on a serving thread's connection pools is left, or it was
    // just cut off: the threads may stop their runtimes.
    drop(release);

    // The socket file is ours: left behind it becomes the stale path the next bind has to probe,
    // and a dead process gets no later chance to clean it up. Already gone is not a failure — the
    // cleanup goal is met either way.
    #[cfg(unix)]
    if let ListenAddr::Unix(path) = &addr {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => warn!("Failed to remove unix socket {}: {error}", path.display()),
        }
    }

    failure.map_or(Ok(Shutdown { severed }), Err)
}

/// Bind a filesystem unix socket, removing a leftover only when nothing answers on it.
///
/// A path that is already a socket is not always a reason to stop: a process killed before it
/// could clean up — SIGKILL, a panic past the shutdown path — leaves the file behind, and
/// treating that as fatal means every `skyzen dev` restart needs a manual `rm`. So `AddrInUse`
/// earns the path one probe: a peer that answers is a live server whose socket binding must
/// not be stolen, while a refused connection means nothing is listening and the file is safe
/// to remove. Any other outcome retries the bind unchanged — either the path raced away, or
/// the probe proved nothing and the first error is the honest one.
///
/// A failure names the path, since the OS error alone does not.
#[cfg(unix)]
fn bind_unix_socket(path: &std::path::Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::{
        io::ErrorKind,
        os::unix::net::{UnixListener, UnixStream},
    };

    let bound = match UnixListener::bind(path) {
        Err(bind_error) if bind_error.kind() == ErrorKind::AddrInUse => {
            match UnixStream::connect(path) {
                Ok(_) => Err(std::io::Error::new(
                    ErrorKind::AddrInUse,
                    "a server is already listening on it",
                )),
                Err(connect_error) => match connect_error.kind() {
                    ErrorKind::ConnectionRefused => {
                        std::fs::remove_file(path)?;
                        warn!("Removed a stale unix socket at {}", path.display());
                        UnixListener::bind(path)
                    }
                    ErrorKind::NotFound => UnixListener::bind(path),
                    _ => Err(bind_error),
                },
            }
        }
        result => result,
    };
    bound.map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("could not bind unix socket {}: {error}", path.display()),
        )
    })
}

/// Hand this process over to the AWS Lambda adapter.
///
/// Without the `lambda` feature there is nothing to hand over to, and the honest answer is to
/// refuse: a Skyzen server that binds a socket inside Lambda is a function that times out on every
/// invocation with nothing in its logs to explain why.
#[cfg(feature = "lambda")]
fn run_on_lambda<Setup, F, C>(
    _options: LaunchOptions,
    setup: impl FnOnce(ServicePools) -> Setup,
) -> Result<core::convert::Infallible, StartupError>
where
    Setup: Future<Output = (F, C)>,
    F: EndpointFactory,
    C: ConsumerSet,
{
    // The adapter's runtime is single-threaded, so the endpoint is built once, on the thread that
    // serves every invocation. That thread runs tokio, which zenwave's per-thread pools cannot run
    // on, so the HTTP service clients stay on zenwave's driver thread.
    skyzen_lambda::run(|| async move {
        let (factory, consumers) = setup(ServicePools { driver: None }).await;
        (factory.endpoint().await, LambdaConsumers(consumers))
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
fn run_on_lambda<Setup, F, C>(
    _options: LaunchOptions,
    _setup: impl FnOnce(ServicePools) -> Setup,
) -> Result<core::convert::Infallible, StartupError>
where
    Setup: Future<Output = (F, C)>,
    F: EndpointFactory,
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
async fn serve_functions_host<F, C>(
    factory: F,
    consumers: C,
    options: &LaunchOptions,
    driver: http_kit_io::compio::Compio,
    port: u16,
) -> Result<Shutdown, RuntimeFailure>
where
    F: EndpointFactory,
    F::Endpoint: ServedRoutes,
    C: ConsumerSet,
{
    let host_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let addr = match &options.listen {
        Some(ListenOverride {
            addr,
            explicit: true,
        }) if *addr != ListenAddr::Tcp(host_addr) => {
            warn!(
                requested = %addr,
                functions_host = %host_addr,
                "--listen overrides the port the Azure Functions host is waiting on; the host \
                 will not reach this process"
            );
            addr.clone()
        }
        _ => ListenAddr::Tcp(host_addr),
    };

    info!(
        port,
        triggers = options.azure_queue_triggers.len(),
        "Serving the Azure Functions host as a custom handler"
    );

    // Every thread mounts the triggers over its own router and reaches the one queue handler
    // through the shared set. Under the Functions host the platform pushes every message, so
    // nothing here polls.
    let consumers = Arc::new(consumers);
    let triggers = options.azure_queue_triggers;
    serve_cores(
        factory,
        move |endpoint| {
            azure::mount_shared(endpoint, Arc::clone(&consumers), triggers)
                .map_err(RuntimeFailure::Mount)
        },
        (),
        addr,
        ListenerSetup {
            server: options.server,
            // `launch` refused a TLS pair before this path runs, since the Functions host speaks
            // plaintext to its custom handler, and refuses HTTP/3 the same way.
            tls: None,
            http3: None,
            // The host owns the sockets, so nothing here is steered; `launch` refuses forcing it.
            steering: Steering::Off,
            io: options.io,
            driver,
        },
    )
    .await
}

fn server_addr() -> ListenAddr {
    std::env::var("SKYZEN_ADDRESS").map_or_else(
        |_| ListenAddr::Tcp(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
        |addr| {
            // Use the provided address by default
            addr.parse()
                .unwrap_or_else(|error| panic!("Invalid SKYZEN_ADDRESS value: {error}"))
        },
    )
}

/// How many native connections may be open at once, across every serving thread. Each thread
/// takes an equal share and closes a connection accepted past it instead of queueing unbounded
/// tasks.
const MAX_CONNECTIONS: usize = 8_192;

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::bind_unix_socket;
    use super::{
        apply_cli_overrides, drain_connections, server_addr, ListenAddr, ListenAddrError,
        ListenOverride, Platform, StartupError, LAMBDA_RUNTIME_API_ENV,
    };
    use super::{apply_steering_override, Steering};
    use crate::runtime::azure::CUSTOM_HANDLER_PORT_ENV;
    use serial_test::serial;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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

    #[test]
    #[serial]
    fn server_addr_defaults_to_random_localhost_port() {
        let _guard = EnvGuard::clear();

        assert_eq!(
            server_addr(),
            ListenAddr::Tcp(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        );
    }

    #[test]
    #[serial]
    fn server_addr_uses_environment_override() {
        let _guard = EnvGuard::set("127.0.0.1:4012");

        assert_eq!(
            server_addr(),
            ListenAddr::Tcp(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4012))
        );
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn server_addr_accepts_a_unix_environment_value() {
        let _guard = EnvGuard::set("unix:/tmp/skyzen-env.sock");

        assert_eq!(
            server_addr(),
            ListenAddr::Unix(std::path::PathBuf::from("/tmp/skyzen-env.sock"))
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

    #[test]
    fn listen_addr_parses_tcp_and_rejects_garbage() {
        assert_eq!(
            "127.0.0.1:8080"
                .parse::<ListenAddr>()
                .expect("a valid socket address"),
            ListenAddr::Tcp("127.0.0.1:8080".parse().unwrap())
        );
        assert!(matches!(
            "not-an-address".parse::<ListenAddr>(),
            Err(ListenAddrError::NotAnAddress(_))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn listen_addr_parses_every_unix_scheme_form() {
        for (value, path) in [
            ("unix:/tmp/app.sock", "/tmp/app.sock"),
            ("unix:///tmp/app.sock", "/tmp/app.sock"),
            ("unix:rel/app.sock", "rel/app.sock"),
        ] {
            assert_eq!(
                value.parse::<ListenAddr>().expect("a valid unix path"),
                ListenAddr::Unix(path.into()),
                "{value}"
            );
        }
        assert!(matches!(
            "unix:".parse::<ListenAddr>(),
            Err(ListenAddrError::EmptyUnixPath)
        ));
    }

    #[test]
    #[cfg(not(unix))]
    fn listen_addr_refuses_unix_paths_where_they_do_not_exist() {
        assert!(matches!(
            "unix:/tmp/app.sock".parse::<ListenAddr>(),
            Err(ListenAddrError::UnixUnsupported)
        ));
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn apply_cli_overrides_accepts_a_unix_listen_path() {
        let _guard = EnvGuard::clear();

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--listen".to_owned(),
                "unix:/tmp/app.sock".to_owned(),
            ]),
            Some(ListenOverride {
                addr: ListenAddr::Unix("/tmp/app.sock".into()),
                explicit: true,
            })
        );
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn tcp_flags_cannot_adjust_a_unix_base_address() {
        let _guard = EnvGuard::set("unix:/tmp/env.sock");

        // `--port` adjusts a TCP address; a unix `SKYZEN_ADDRESS` keeps its path and the
        // flag is ignored with a warning rather than silently switching transports.
        assert_eq!(
            apply_cli_overrides(["skyzen".to_owned(), "--port=8080".to_owned()]),
            Some(ListenOverride {
                addr: ListenAddr::Unix("/tmp/env.sock".into()),
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
        use super::{AnyExecutor, SmolGlobal};
        use crate::runtime::{context::ShutdownGuard, WorkerContext};
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

    /// A socket path no other test or run is using, short enough for the platform's limit.
    #[cfg(unix)]
    fn unique_socket_path() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("skyzen-test-{}-{nanos}.sock", std::process::id()))
    }

    #[test]
    #[cfg(unix)]
    fn bind_unix_socket_takes_a_fresh_path() {
        let path = unique_socket_path();

        let listener = bind_unix_socket(&path).expect("nothing is in the way");

        drop(listener);
        std::fs::remove_file(&path).expect("the test created the socket");
    }

    #[test]
    #[cfg(unix)]
    fn bind_unix_socket_reclaims_a_stale_socket() {
        let path = unique_socket_path();
        // A dead listener's socket file stays on the filesystem; connect refuses, so the
        // bind must remove it and take the path rather than report `AddrInUse`.
        drop(std::os::unix::net::UnixListener::bind(&path).expect("bind"));

        let listener = bind_unix_socket(&path).expect("a stale socket is reclaimed");

        drop(listener);
        std::fs::remove_file(&path).expect("the test created the socket");
    }

    #[test]
    #[cfg(unix)]
    fn bind_unix_socket_never_steals_a_live_socket() {
        let path = unique_socket_path();
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");

        let error = bind_unix_socket(&path).expect_err("a live server keeps its socket");

        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(path.exists(), "the live socket must not be unlinked");
        drop(listener);
        std::fs::remove_file(&path).expect("the test created the socket");
    }

    fn server_override(args: &[&str]) -> super::HttpServer {
        super::apply_server_override(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    #[serial]
    fn the_default_server_is_h1() {
        let _env = EnvGuard::clear_var("SKYZEN_SERVER");
        assert_eq!(server_override(&["app"]), super::HttpServer::H1);
    }

    #[test]
    #[serial]
    fn the_server_flag_beats_the_environment() {
        let _env = EnvGuard::set_var("SKYZEN_SERVER", "h1");
        assert_eq!(
            server_override(&["app", "--server", "hyper"]),
            super::HttpServer::Hyper
        );
        assert_eq!(
            server_override(&["app", "--server=hyper"]),
            super::HttpServer::Hyper
        );
        assert_eq!(server_override(&["app"]), super::HttpServer::H1);
    }

    #[test]
    #[serial]
    fn the_server_environment_variable_selects_hyper() {
        let _env = EnvGuard::set_var("SKYZEN_SERVER", "hyper");
        assert_eq!(server_override(&["app"]), super::HttpServer::Hyper);
    }

    #[test]
    #[serial]
    #[should_panic(expected = "not a server")]
    fn an_unknown_server_stops_the_process() {
        let _env = EnvGuard::clear_var("SKYZEN_SERVER");
        let _ = server_override(&["app", "--server", "apache"]);
    }

    /// `apply_tls_overrides` through the same iterator shape `#[skyzen::main]` hands it:
    /// a leading binary name followed by the real arguments.
    fn tls_overrides(args: &[&str]) -> Option<super::TlsFiles> {
        super::apply_tls_overrides(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    #[serial]
    fn tls_overrides_unset_means_cleartext() {
        let _cert = EnvGuard::clear_var("SKYZEN_TLS_CERT");
        let _key = EnvGuard::clear_var("SKYZEN_TLS_KEY");
        assert_eq!(tls_overrides(&["app"]), None);
    }

    #[test]
    #[serial]
    fn tls_overrides_reads_the_env_pair() {
        let _cert = EnvGuard::set_var("SKYZEN_TLS_CERT", "cert.pem");
        let _key = EnvGuard::set_var("SKYZEN_TLS_KEY", "key.pem");
        assert_eq!(
            tls_overrides(&["app"]),
            Some(super::TlsFiles {
                cert: "cert.pem".into(),
                key: "key.pem".into(),
            })
        );
    }

    #[test]
    #[serial]
    fn tls_overrides_flags_beat_env() {
        let _cert = EnvGuard::set_var("SKYZEN_TLS_CERT", "env-cert.pem");
        let _key = EnvGuard::clear_var("SKYZEN_TLS_KEY");
        assert_eq!(
            tls_overrides(&["app", "--tls-cert", "flag.pem", "--tls-key=k.pem"]),
            Some(super::TlsFiles {
                cert: "flag.pem".into(),
                key: "k.pem".into(),
            })
        );
    }

    #[test]
    #[serial]
    fn tls_overrides_falls_back_per_file() {
        let _cert = EnvGuard::clear_var("SKYZEN_TLS_CERT");
        let _key = EnvGuard::set_var("SKYZEN_TLS_KEY", "env-key.pem");
        assert_eq!(
            tls_overrides(&["app", "--tls-cert=c.pem"]),
            Some(super::TlsFiles {
                cert: "c.pem".into(),
                key: "env-key.pem".into(),
            })
        );
    }

    #[test]
    #[serial]
    #[should_panic(expected = "without a key")]
    fn tls_overrides_cert_without_key_panics() {
        let _cert = EnvGuard::set_var("SKYZEN_TLS_CERT", "cert.pem");
        let _key = EnvGuard::clear_var("SKYZEN_TLS_KEY");
        let _ = tls_overrides(&["app"]);
    }

    #[test]
    #[serial]
    #[should_panic(expected = "without a certificate")]
    fn tls_overrides_key_without_cert_panics() {
        let _cert = EnvGuard::clear_var("SKYZEN_TLS_CERT");
        let _key = EnvGuard::set_var("SKYZEN_TLS_KEY", "key.pem");
        let _ = tls_overrides(&["app"]);
    }

    #[test]
    #[serial]
    #[should_panic(expected = "expects a PEM path")]
    fn tls_overrides_cert_flag_without_a_value_panics() {
        let _cert = EnvGuard::clear_var("SKYZEN_TLS_CERT");
        let _key = EnvGuard::clear_var("SKYZEN_TLS_KEY");
        let _ = tls_overrides(&["app", "--tls-cert"]);
    }

    #[test]
    #[serial]
    #[should_panic(expected = "expects a PEM path")]
    fn tls_overrides_key_flag_without_a_value_panics() {
        let _cert = EnvGuard::clear_var("SKYZEN_TLS_CERT");
        let _key = EnvGuard::clear_var("SKYZEN_TLS_KEY");
        let _ = tls_overrides(&["app", "--tls-key"]);
    }

    #[test]
    #[serial]
    #[should_panic(expected = "expects a PEM path")]
    fn tls_overrides_flag_eating_another_flag_panics() {
        let _cert = EnvGuard::clear_var("SKYZEN_TLS_CERT");
        let _key = EnvGuard::clear_var("SKYZEN_TLS_KEY");
        let _ = tls_overrides(&["app", "--tls-cert", "--tls-key", "k.pem"]);
    }

    /// A non-UTF-8 path is still a configured TLS file: reading it as absent would boot the
    /// server plaintext under a configuration that promised encryption. The path is unusable but
    /// it must still register as a TLS setting: either a pair or a startup error, never `None`.
    #[test]
    #[serial]
    #[cfg(unix)]
    fn a_non_utf8_tls_env_value_still_counts_as_configured() {
        use std::os::unix::ffi::OsStrExt;
        let _key = EnvGuard::clear_var("SKYZEN_TLS_KEY");
        std::env::set_var(
            "SKYZEN_TLS_CERT",
            std::ffi::OsStr::from_bytes(b"\xff\xfe.pem"),
        );
        let pair = tls_overrides(&["app", "--tls-key", "k.pem"]);
        std::env::remove_var("SKYZEN_TLS_CERT");
        let pair = pair.expect("a non-UTF-8 path is still a certificate setting");
        assert_eq!(pair.cert.as_os_str().as_bytes(), b"\xff\xfe.pem");
    }

    fn http3_requested(args: &[&str]) -> bool {
        super::apply_http3_override(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    #[serial]
    fn http3_is_not_requested_by_default() {
        let _env = EnvGuard::clear_var("SKYZEN_HTTP3");
        assert!(!http3_requested(&["app"]));
    }

    #[test]
    #[serial]
    fn http3_is_requested_by_the_flag_or_the_variable() {
        let cleared = EnvGuard::clear_var("SKYZEN_HTTP3");
        assert!(http3_requested(&["app", "--http3"]));
        drop(cleared);
        for value in ["true", "1"] {
            let _env = EnvGuard::set_var("SKYZEN_HTTP3", value);
            assert!(http3_requested(&["app"]), "{value}");
        }
        for value in ["false", "0"] {
            let _env = EnvGuard::set_var("SKYZEN_HTTP3", value);
            assert!(!http3_requested(&["app"]), "{value}");
        }
    }

    #[test]
    #[serial]
    #[should_panic(expected = "SKYZEN_HTTP3 must be")]
    fn a_malformed_http3_variable_stops_the_process() {
        let _env = EnvGuard::set_var("SKYZEN_HTTP3", "maybe");
        let _ = http3_requested(&["app"]);
    }

    fn steering_requested(args: &[&str]) -> Steering {
        apply_steering_override(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    #[serial]
    fn steering_is_auto_by_default_and_the_flag_beats_the_variable() {
        let cleared = EnvGuard::clear_var("SKYZEN_STEERING");
        assert_eq!(steering_requested(&["app"]), Steering::Auto);
        assert_eq!(
            steering_requested(&["app", "--steering", "on"]),
            Steering::On
        );
        drop(cleared);
        let _env = EnvGuard::set_var("SKYZEN_STEERING", "on");
        assert_eq!(steering_requested(&["app"]), Steering::On);
        assert_eq!(
            steering_requested(&["app", "--steering=off"]),
            Steering::Off
        );
    }

    #[test]
    #[serial]
    #[should_panic(expected = "invalid SKYZEN_STEERING")]
    fn a_malformed_steering_variable_stops_the_process() {
        let _env = EnvGuard::set_var("SKYZEN_STEERING", "maybe");
        let _ = steering_requested(&["app"]);
    }
}
