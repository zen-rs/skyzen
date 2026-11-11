//! The AWS SDK's I/O, on runtimes that need no tokio context.
//!
//! The SDK reaches the outside world through two extension points: an [`HttpClient`] for its
//! requests and an [`AsyncSleep`] for its retry back-off and timeouts. Its own implementations of
//! both (`rt-tokio`, and `default-https-client`'s hyper) need a tokio runtime, which the compio
//! threads the native server runs on do not have.
//!
//! - [`ZenwaveHttpClient`] sends each request through a zenwave [`NativeBackend`]: a `Send + Sync`
//!   handle onto a connection pool that lives on one thread's compio runtime, so an SDK future —
//!   which must be `Send` — stays `Send` while every socket stays on that runtime. The pools run on
//!   zenwave's own driver thread ([`ZenwaveHttpClient::system`]) or on the thread that built the
//!   client ([`ZenwaveHttpClient::system_on_current_thread`]), which is how each of Skyzen's
//!   serving threads keeps connections of its own.
//! - [`AsyncIoSleep`] waits on an async-io timer, which any executor can poll.
//!
//! [`config_loader`] wires both into aws-config's loader, and [`load_config`] loads the ambient
//! configuration through it — what every `from_env` constructor in this crate and in `skyzen-s3`
//! calls.

use core::{fmt, future::Future, time::Duration};
use std::{collections::HashMap, sync::Arc};

use aws_config::{
    environment::EnvironmentVariableCredentialsProvider,
    meta::credentials::CredentialsProviderChain, BehaviorVersion, ConfigLoader, SdkConfig,
};
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_smithy_async::rt::sleep::{AsyncSleep, Sleep};
use aws_smithy_runtime_api::client::{
    http::{
        HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
    },
    orchestrator::{HttpRequest, HttpResponse},
    result::ConnectorError,
    runtime_components::RuntimeComponents,
};
use aws_smithy_types::body::SdkBody;
use http_kit::{Body, Endpoint as _};
use zenwave::{
    backend::{NativeBackend, NativeIo},
    Transport, TransportBuilder,
};

use crate::credential_process::CredentialProcessProvider;

/// The AWS SDK's [`HttpClient`], on zenwave [`NativeBackend`]s.
///
/// Clones share the backends and their connection pools. A `4xx` or `5xx` answer is handed to the
/// SDK as the response it is, so the SDK reads the service's error from its body as it does on any
/// client.
///
/// # Where the connections live
///
/// `P` names the thread the connection pools run on:
///
/// - [`SharedDriver`] ([`system`](Self::system), [`new`](Self::new)): a zenwave driver thread,
///   started by the first request, for callers on no runtime of their own.
/// - [`ThreadPools`] ([`system_on_current_thread`](Self::system_on_current_thread),
///   [`on_current_thread`](Self::on_current_thread)): the compio runtime of the thread that built
///   the client. A thread-per-core server builds one on each serving thread, and a request made
///   there never leaves it. A request made on another thread — a background task on the global
///   executor — still reaches the pool, over a channel to its thread; once that thread's runtime
///   has stopped, it fails.
///
/// # Timeouts
///
/// The SDK's operation and attempt timeouts run on [`AsyncIoSleep`] and bound every call. The
/// connect and read timeouts in the [`HttpConnectorSettings`] the SDK passes are zenwave's own:
/// each distinct pair gets a [`NativeBackend`] over a [`Transport`] carrying exactly those
/// timeouts, built on first use and kept, so the SDK's clients with equal settings share one pool,
/// as they do on aws-smithy's own hyper client.
///
/// - The connect timeout bounds one connection attempt: DNS, TCP, a proxy tunnel and TLS. It
///   reaches the SDK as a [`ConnectorError`] timeout.
/// - The read timeout bounds each wait for bytes of the response, head and body. A head that
///   takes longer is a [`ConnectorError`] timeout; a body that stalls fails its read with an
///   [`std::io::ErrorKind::TimedOut`] error wrapping [`zenwave::Error::Timeout`].
#[derive(Clone)]
pub struct ZenwaveHttpClient<P = SharedDriver> {
    /// Describes the base transport, proxy and trust roots, each backend's timeouts go on top of.
    transport: Arc<dyn Fn() -> TransportBuilder + Send + Sync>,
    /// One backend per distinct settings, shared by every clone.
    backends: Backends,
    /// Where a backend's pool is started.
    pools: P,
}

impl<P: fmt::Debug> fmt::Debug for ZenwaveHttpClient<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZenwaveHttpClient")
            .field("backends", &self.backends)
            .field("pools", &self.pools)
            .finish_non_exhaustive()
    }
}

/// Where a [`ZenwaveHttpClient`]'s connection pools run: [`SharedDriver`] or [`ThreadPools`].
pub trait Pools: sealed::Sealed {}

/// What the sealed half of [`Pools`] names: public items no path outside this module reaches.
mod sealed {
    use core::{fmt, future::Future, time::Duration};
    use std::{
        collections::{hash_map::Entry, HashMap},
        sync::{Arc, Mutex, PoisonError},
    };

    use zenwave::{backend::NativeBackend, Transport};

    /// How a pool is started; sealed, because the two kinds are the only two places a pool runs.
    pub trait Sealed: Clone + fmt::Debug + Send + Sync + 'static {
        /// The backend for `timeouts`: the one in `backends`, or one built over `transport()` and
        /// kept there.
        fn backend(
            &self,
            backends: &Backends,
            timeouts: Timeouts,
            transport: impl FnOnce() -> Result<Transport, Unavailable> + Send,
        ) -> impl Future<Output = Result<NativeBackend, Unavailable>> + Send + 'static;
    }

    /// The part of `HttpConnectorSettings` a backend depends on, which is not `Hash` itself.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Timeouts {
        pub connect: Option<Duration>,
        pub read: Option<Duration>,
    }

    /// One backend per distinct settings. The SDK runs a call on whichever thread polls it, so the
    /// map needs a lock; it is held only to look up, insert, or build one backend.
    #[derive(Clone, Debug, Default)]
    pub struct Backends(Arc<Mutex<HashMap<Timeouts, NativeBackend>>>);

    impl Backends {
        /// The backend for `timeouts`, if one was built.
        pub fn get(&self, timeouts: Timeouts) -> Option<NativeBackend> {
            // The map is never left half-written, so a lock poisoned by an unrelated panic is
            // still a consistent cache.
            let backends = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            backends.get(&timeouts).cloned()
        }

        /// The backend for `timeouts`, built by `build` under the lock when there is none, so two
        /// threads asking for the same settings build one backend. A failed build is not cached.
        pub fn get_or_build(
            &self,
            timeouts: Timeouts,
            build: impl FnOnce() -> Result<NativeBackend, Unavailable>,
        ) -> Result<NativeBackend, Unavailable> {
            let mut backends = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            match backends.entry(timeouts) {
                Entry::Occupied(entry) => Ok(entry.get().clone()),
                Entry::Vacant(entry) => Ok(entry.insert(build()?).clone()),
            }
        }

        /// Keep `backend` for `timeouts` unless another was kept first, and return the one kept.
        pub fn keep(&self, timeouts: Timeouts, backend: NativeBackend) -> NativeBackend {
            let mut backends = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            backends.entry(timeouts).or_insert(backend).clone()
        }
    }

    /// Why a connector has no backend to send on.
    #[derive(Debug, Clone)]
    pub enum Unavailable {
        /// The transport could not be built, such as when the system trust store cannot be read.
        Transport(Arc<zenwave::Error>),
        /// The thread the client's pools run on has stopped its runtime.
        Stopped,
    }

    impl fmt::Display for Unavailable {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Transport(error) => {
                    write!(f, "the AWS SDK's transport cannot be built: {error}")
                }
                Self::Stopped => f.write_str(
                    "the thread whose runtime runs this client's connection pools has stopped",
                ),
            }
        }
    }

    impl core::error::Error for Unavailable {
        fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
            match self {
                Self::Transport(error) => Some(&**error),
                Self::Stopped => None,
            }
        }
    }
}

use sealed::{Backends, Timeouts, Unavailable};

/// Pools on zenwave's own driver thread, one per transport, started by the first request.
#[derive(Debug, Clone, Copy, Default)]
pub struct SharedDriver;

impl Pools for SharedDriver {}

impl sealed::Sealed for SharedDriver {
    fn backend(
        &self,
        backends: &Backends,
        timeouts: Timeouts,
        transport: impl FnOnce() -> Result<Transport, Unavailable> + Send,
    ) -> impl Future<Output = Result<NativeBackend, Unavailable>> + Send + 'static {
        // Built synchronously, under the lock: every backend starts a driver thread of its own,
        // so two racing requests must not build two.
        let backend = backends.get_or_build(timeouts, || Ok(NativeBackend::new(transport()?)));
        core::future::ready(backend)
    }
}

/// Pools on the compio runtime of the thread a client was built on.
///
/// A task on that runtime builds each pool, so a pool for settings first asked for later — or
/// from another thread — still starts there.
#[derive(Debug, Clone)]
pub struct ThreadPools {
    builds: async_channel::Sender<Build>,
}

/// A request to the pool-building task for one set of settings.
struct Build {
    timeouts: Timeouts,
    transport: Transport,
    reply: async_channel::Sender<NativeBackend>,
}

impl ThreadPools {
    /// Start the pool-building task on the current thread's compio runtime, its pools driven by
    /// `io`.
    fn start<I: NativeIo>(io: I) -> Self {
        let (builds, requests) = async_channel::unbounded::<Build>();
        // The task holds no handle to the runtime it runs on, which would keep that runtime from
        // ever being torn down; it reaches it through `with_current` whenever it builds. It ends
        // when the last client clone is dropped.
        compio::runtime::spawn(async move {
            let mut built = HashMap::new();
            while let Ok(Build {
                timeouts,
                transport,
                reply,
            }) = requests.recv().await
            {
                let backend = built
                    .entry(timeouts)
                    .or_insert_with(|| {
                        compio::runtime::Runtime::with_current(|runtime| {
                            NativeBackend::on_current_thread(transport, io.clone(), runtime)
                        })
                    })
                    .clone();
                // A caller that gave up waiting has nothing left to hand the backend to.
                let _ = reply.try_send(backend);
            }
        })
        .detach();
        Self { builds }
    }
}

impl Pools for ThreadPools {}

impl sealed::Sealed for ThreadPools {
    fn backend(
        &self,
        backends: &Backends,
        timeouts: Timeouts,
        transport: impl FnOnce() -> Result<Transport, Unavailable> + Send,
    ) -> impl Future<Output = Result<NativeBackend, Unavailable>> + Send + 'static {
        // The cached backend, or the transport to build one over: built here, so the future
        // holds no closure.
        let lookup = backends.get(timeouts).ok_or_else(transport);
        let builds = self.builds.clone();
        let backends = backends.clone();
        async move {
            let transport = match lookup {
                Ok(backend) => return Ok(backend),
                Err(transport) => transport?,
            };
            let (reply, built) = async_channel::bounded(1);
            builds
                .send(Build {
                    timeouts,
                    transport,
                    reply,
                })
                .await
                .map_err(|_| Unavailable::Stopped)?;
            let backend = built.recv().await.map_err(|_| Unavailable::Stopped)?;
            Ok(backends.keep(timeouts, backend))
        }
    }
}

impl ZenwaveHttpClient {
    /// A client whose backends are built on the transport `transport` describes, with the SDK's
    /// timeouts added. `transport` runs once per distinct set of timeouts.
    #[must_use]
    pub fn new(transport: impl Fn() -> TransportBuilder + Send + Sync + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
            backends: Backends::default(),
            pools: SharedDriver,
        }
    }

    /// A client on the system transport — the OS proxy settings and trust store — whose pools
    /// run on zenwave's compio driver threads.
    #[must_use]
    pub fn system() -> Self {
        Self::new(Transport::builder)
    }
}

impl ZenwaveHttpClient<ThreadPools> {
    /// A client whose backends are built on the transport `transport` describes, with the SDK's
    /// timeouts added, and whose pools run on the current thread's compio runtime, driven by `io`.
    ///
    /// # Panics
    ///
    /// When the current thread is not running a compio runtime.
    #[must_use]
    pub fn on_current_thread<I: NativeIo>(
        transport: impl Fn() -> TransportBuilder + Send + Sync + 'static,
        io: I,
    ) -> Self {
        Self {
            transport: Arc::new(transport),
            backends: Backends::default(),
            pools: ThreadPools::start(io),
        }
    }

    /// [`on_current_thread`](Self::on_current_thread) on the system transport.
    ///
    /// # Panics
    ///
    /// When the current thread is not running a compio runtime.
    #[must_use]
    pub fn system_on_current_thread<I: NativeIo>(io: I) -> Self {
        Self::on_current_thread(Transport::builder, io)
    }
}

impl<P: Pools> ZenwaveHttpClient<P> {
    /// The transport for `timeouts`. One that cannot be built, such as one whose system trust
    /// store cannot be read, is not cached: the error is returned, and the next call tries again.
    fn transport(&self, timeouts: Timeouts) -> Result<Transport, Unavailable> {
        let mut transport = (self.transport)();
        if let Some(connect) = timeouts.connect {
            transport = transport.connect_timeout(connect);
        }
        if let Some(read) = timeouts.read {
            transport = transport.read_timeout(read);
        }
        transport
            .build()
            .map_err(|error| Unavailable::Transport(Arc::new(error)))
    }

    /// The backend for `timeouts`, built on first use.
    fn backend(
        &self,
        timeouts: Timeouts,
    ) -> impl Future<Output = Result<NativeBackend, Unavailable>> + Send + 'static + use<'_, P>
    {
        // `transport` runs before this returns, so the future borrows nothing.
        sealed::Sealed::backend(&self.pools, &self.backends, timeouts, move || {
            self.transport(timeouts)
        })
    }
}

impl<P: Pools> HttpClient for ZenwaveHttpClient<P> {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(ZenwaveConnector {
            client: self.clone(),
            timeouts: Timeouts {
                connect: settings.connect_timeout(),
                read: settings.read_timeout(),
            },
        })
    }
}

/// An [`HttpConnector`] on the [`NativeBackend`] for one set of [`HttpConnectorSettings`], looked
/// up — and built on first use — by each call.
#[derive(Debug, Clone)]
struct ZenwaveConnector<P> {
    client: ZenwaveHttpClient<P>,
    timeouts: Timeouts,
}

impl<P: Pools> HttpConnector for ZenwaveConnector<P> {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let backend = self.client.backend(self.timeouts);
        HttpConnectorFuture::new(async move {
            let mut backend = backend.await.map_err(|unavailable| {
                if let Unavailable::Transport(error) = &unavailable {
                    tracing::error!("the AWS SDK's transport cannot be built: {error}");
                }
                ConnectorError::other(Box::new(unavailable), None)
            })?;
            let mut request = request
                .try_into_http1x()
                .map_err(|error| ConnectorError::user(error.into()))?
                .map(Body::new);
            let response = backend
                .respond(&mut request)
                .await
                .map_err(connector_error)?;
            HttpResponse::try_from(response.map(SdkBody::from_body_1_x))
                .map_err(|error| ConnectorError::other(error.into(), None))
        })
    }
}

/// How the SDK should treat a request that never got an answer: a timeout as a timeout, a
/// transport or I/O failure as retryable, anything else — a malformed request, a TLS or proxy
/// refusal — as final.
fn connector_error(error: zenwave::Error) -> ConnectorError {
    match error {
        zenwave::Error::Timeout => ConnectorError::timeout(error.into()),
        error if error.is_transient() => ConnectorError::io(error.into()),
        error => ConnectorError::other(error.into(), None),
    }
}

/// The SDK's [`AsyncSleep`], on an async-io timer.
///
/// An SDK sleep must be `Send`, which a thread-per-core driver's timer is not; async-io's timers
/// live on its own reactor thread and are polled from any executor.
#[derive(Debug, Clone, Copy, Default)]
pub struct AsyncIoSleep;

impl AsyncSleep for AsyncIoSleep {
    fn sleep(&self, duration: Duration) -> Sleep {
        Sleep::new(async move {
            async_io::Timer::after(duration).await;
        })
    }
}

/// aws-config's loader for the latest behavior version, with its requests on `http` and its
/// timers on [`AsyncIoSleep`].
///
/// Every credential and region provider the loader builds — IMDS, SSO, STS — rides the same
/// client, so the loaded [`SdkConfig`] needs no tokio context anywhere.
#[must_use]
pub fn config_loader(http: ZenwaveHttpClient) -> ConfigLoader {
    aws_config::defaults(BehaviorVersion::latest())
        .http_client(http)
        .sleep_impl(AsyncIoSleep)
}

/// The ambient AWS configuration — environment, profile files, instance metadata — loaded through
/// [`config_loader`] with [`ZenwaveHttpClient::system`].
///
/// The credentials come from the default chain with [`CredentialProcessProvider`] spliced in
/// after the environment variables, which the default chain tries first, so a profile's
/// `credential_process` is honoured and a profile without one falls through to the rest of the
/// chain. aws-config's own `credential_process` support is compiled out, since it needs tokio.
///
/// # Panics
///
/// If zenwave cannot start its driver thread.
pub async fn load_config() -> SdkConfig {
    let config = config_loader(ZenwaveHttpClient::system()).load().await;
    let default_chain = config
        .credentials_provider()
        .expect("aws-config's loader always installs a credentials provider");
    let chain = CredentialsProviderChain::first_try(
        "Environment",
        EnvironmentVariableCredentialsProvider::new(),
    )
    .or_else("CredentialProcess", CredentialProcessProvider::new())
    .or_else("Default", default_chain);
    config
        .into_builder()
        .credentials_provider(SharedCredentialsProvider::new(chain))
        .build()
}

#[cfg(all(test, feature = "dynamodb"))]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    };

    use std::time::{Duration, Instant};

    use aws_config::{retry::RetryConfig, timeout::TimeoutConfig, Region};
    use aws_sdk_dynamodb::config::Credentials;
    use base64::Engine as _;
    use http_kit::{Body, Response, StatusCode};
    use skyzen_services::kv::KeyValueStore as _;
    use skyzen_test::{LoopbackServer, ReceivedRequest};

    use super::{config_loader, ZenwaveHttpClient};
    use crate::DynamoKv;

    /// A key and a value in Chinese, Japanese and Korean.
    const KEY: &str = "订单-注文-주문";
    const VALUE: &str = "一杯茶。お茶を一杯。차 한 잔.";

    /// A `DynamoKv` whose SDK client loads its configuration through [`config_loader`] and sends
    /// every request to `server`.
    async fn kv(server: &LoopbackServer) -> DynamoKv {
        kv_with(
            server,
            TimeoutConfig::builder().build(),
            RetryConfig::standard(),
        )
        .await
    }

    /// [`kv`], with the SDK configured with `timeouts` and `retries`.
    async fn kv_with(
        server: &LoopbackServer,
        timeouts: TimeoutConfig,
        retries: RetryConfig,
    ) -> DynamoKv {
        kv_at(format!("http://{}", server.addr()), timeouts, retries).await
    }

    /// [`kv_with`], sending its requests to `endpoint`.
    async fn kv_at(endpoint: String, timeouts: TimeoutConfig, retries: RetryConfig) -> DynamoKv {
        let config = config_loader(ZenwaveHttpClient::system())
            .timeout_config(timeouts)
            .retry_config(retries)
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new(
                "AKIDLOOPBACK",
                "loopback-secret",
                None,
                None,
                "loopback",
            ))
            .endpoint_url(endpoint)
            .load()
            .await;
        DynamoKv::new(aws_sdk_dynamodb::Client::new(&config), "orders", "pk")
    }

    /// A `DynamoDB` JSON answer.
    fn answer(status: StatusCode, body: &serde_json::Value) -> Response {
        let mut response = Response::new(Body::from_bytes(
            serde_json::to_vec(body).expect("a JSON value serializes"),
        ));
        *response.status_mut() = status;
        response.headers_mut().insert(
            http_kit::header::CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/x-amz-json-1.0"),
        );
        response
    }

    fn assert_no_tokio_runtime() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "the test must run with no tokio runtime"
        );
    }

    /// Every dial in `dials` was made on `thread`, and there was at least one.
    fn assert_dialed_on(dials: &[std::thread::ThreadId], thread: std::thread::ThreadId) {
        assert!(
            !dials.is_empty(),
            "a request went out, so a connection was dialed"
        );
        assert!(
            dials.iter().all(|dialed| *dialed == thread),
            "every connection was dialed on {thread:?}, not {dials:?}"
        );
    }

    /// A store built once and rebound on each of two threads — what a serving thread does with
    /// the clients `#[skyzen::main]` builds — sends every request on a connection that thread
    /// dialed and drives, and the server sees every request.
    #[compio::test]
    async fn each_thread_sends_on_connections_it_dials_itself() {
        let (seen, received) = mpsc::channel::<ReceivedRequest>();
        let server = LoopbackServer::start(move |request| {
            seen.send(request).expect("the test is still listening");
            answer(StatusCode::OK, &serde_json::json!({}))
        });
        let shared = kv(&server).await;

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    let runtime = compio::runtime::Runtime::new().expect("a compio runtime");
                    runtime.block_on(async move {
                        let (io, dials) = skyzen_test::DialRecorder::new();
                        let kv = shared
                            .with_http_client(ZenwaveHttpClient::system_on_current_thread(io));
                        for _ in 0..2 {
                            kv.put(KEY, VALUE.as_bytes())
                                .await
                                .expect("the put should succeed");
                        }
                        (std::thread::current().id(), dials.taken())
                    })
                })
            })
            .collect();

        for thread in threads {
            let (sender, dials) = thread.join().expect("the thread finishes");
            assert_dialed_on(&dials, sender);
        }
        let seen = received.try_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 4, "two puts from each thread");
        for request in seen {
            let item: serde_json::Value =
                serde_json::from_slice(request.body()).expect("the body is DynamoDB JSON");
            assert_eq!(item["Item"]["pk"]["S"], KEY);
        }
    }

    /// A request made on a thread other than the one a client's pools run on — a post-response
    /// task on the global executor — still succeeds, on a connection the owning thread dials.
    #[compio::test]
    async fn a_request_from_another_thread_goes_out_on_the_owning_threads_pool() {
        let server = LoopbackServer::start(|_| answer(StatusCode::OK, &serde_json::json!({})));
        let shared = kv(&server).await;
        let (built, handed) = mpsc::channel();
        let (stop, stopped) = async_channel::bounded::<core::convert::Infallible>(1);

        let owner = std::thread::spawn(move || {
            let runtime = compio::runtime::Runtime::new().expect("a compio runtime");
            runtime.block_on(async move {
                let (io, dials) = skyzen_test::DialRecorder::new();
                built
                    .send(shared.with_http_client(ZenwaveHttpClient::system_on_current_thread(io)))
                    .expect("the test is waiting for the store");
                // The owner keeps its runtime, and so its pools, running until the test is done.
                let _ = stopped.recv().await;
                (std::thread::current().id(), dials.taken())
            })
        });

        let kv = handed.recv().expect("the owner built the store");
        // This thread runs no compio runtime: it is one of the global executor's.
        std::thread::spawn(move || {
            futures_lite::future::block_on(kv.put(KEY, VALUE.as_bytes()))
                .expect("the put should succeed from another thread");
        })
        .join()
        .expect("the other thread finishes");

        drop(stop);
        let (owner_thread, dials) = owner.join().expect("the owner finishes");
        assert_dialed_on(&dials, owner_thread);
    }

    /// A write leaves through zenwave's native backend, signed and shaped by the SDK, while the
    /// test runs on compio with no tokio runtime.
    #[compio::test]
    async fn a_put_goes_out_on_zenwave_without_a_tokio_runtime() {
        assert_no_tokio_runtime();
        let (seen, received) = mpsc::channel::<ReceivedRequest>();
        let server = LoopbackServer::start(move |request| {
            seen.send(request).expect("the test is still listening");
            answer(StatusCode::OK, &serde_json::json!({}))
        });

        kv(&server)
            .await
            .put(KEY, VALUE.as_bytes())
            .await
            .expect("the put should succeed");

        let request = received.recv().expect("the server saw the request");
        assert_eq!(
            request.headers()["x-amz-target"],
            "DynamoDB_20120810.PutItem"
        );
        assert!(
            request
                .headers()
                .contains_key(http_kit::header::AUTHORIZATION),
            "the request should be signed"
        );
        let item: serde_json::Value =
            serde_json::from_slice(request.body()).expect("the body is DynamoDB JSON");
        assert_eq!(item["TableName"], "orders");
        assert_eq!(item["Item"]["pk"]["S"], KEY);
        assert_eq!(
            item["Item"]["value"]["B"],
            base64::engine::general_purpose::STANDARD.encode(VALUE)
        );
    }

    /// A `500` reaches the SDK as the service's own error response, which it reads, backs off from
    /// on [`super::AsyncIoSleep`] and retries; the retry's answer is the result.
    #[compio::test]
    async fn a_server_error_is_retried_after_a_sleep_and_the_retry_answers() {
        assert_no_tokio_runtime();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let server = LoopbackServer::start(move |_| {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                answer(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &serde_json::json!({
                        "__type": "com.amazonaws.dynamodb.v20120810#InternalServerError",
                        "message": "the partition is being rebalanced",
                    }),
                )
            } else {
                answer(
                    StatusCode::OK,
                    &serde_json::json!({
                        "Item": {
                            "pk": { "S": KEY },
                            "value": { "B": base64::engine::general_purpose::STANDARD.encode(VALUE) },
                        },
                    }),
                )
            }
        });

        let value = kv(&server)
            .await
            .get(KEY)
            .await
            .expect("the retried read should succeed");

        assert_eq!(value.as_deref(), Some(VALUE.as_bytes()));
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one failure, one retry");
    }

    /// How much later than its limit a timed-out call may fail: signing, scheduling and the
    /// timer's own granularity.
    const SLACK: Duration = Duration::from_millis(700);

    /// Fails unless `call` ended as a timeout no sooner than `limit` and within `SLACK` after it,
    /// with `elapsed` measured from the call's start.
    fn assert_timed_out_at(limit: Duration, elapsed: Duration, error: &impl std::fmt::Debug) {
        assert!(
            elapsed >= limit,
            "failed early, after {elapsed:?}: {error:?}"
        );
        assert!(
            elapsed < limit + SLACK,
            "failed late, after {elapsed:?}: {error:?}"
        );
        assert!(
            format!("{error:?}").to_lowercase().contains("timeout"),
            "{error:?}"
        );
    }

    /// The read timeout set on the SDK's configuration reaches the connector and bounds the wait
    /// for the response head, which a server stalls for far longer.
    #[compio::test]
    async fn a_configured_read_timeout_bounds_a_stalled_response() {
        assert_no_tokio_runtime();
        let limit = Duration::from_millis(300);
        let server = LoopbackServer::start(|_| {
            std::thread::sleep(Duration::from_secs(5));
            answer(StatusCode::OK, &serde_json::json!({}))
        });

        let started = Instant::now();
        let error = kv_with(
            &server,
            TimeoutConfig::builder().read_timeout(limit).build(),
            RetryConfig::disabled(),
        )
        .await
        .get(KEY)
        .await
        .expect_err("a stalled server must time the call out");

        assert_timed_out_at(limit, started.elapsed(), &error);
    }

    /// The connect timeout bounds a connection that never completes its TLS handshake: a peer that
    /// accepts the TCP connection and says nothing.
    #[compio::test]
    async fn a_configured_connect_timeout_bounds_a_stalled_response() {
        assert_no_tokio_runtime();
        let limit = Duration::from_millis(300);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a silent listener");
        let addr = listener.local_addr().expect("the listener has an address");
        // Holds every accepted connection open and silent until the test ends.
        let (stop, stopped) = mpsc::channel::<()>();
        let silent = std::thread::spawn(move || {
            let mut held = Vec::new();
            listener
                .set_nonblocking(true)
                .expect("a non-blocking listener");
            while stopped.try_recv() == Err(mpsc::TryRecvError::Empty) {
                match listener.accept() {
                    Ok((stream, _)) => held.push(stream),
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            drop(held);
        });

        let started = Instant::now();
        let error = kv_at(
            format!("https://{addr}"),
            TimeoutConfig::builder().connect_timeout(limit).build(),
            RetryConfig::disabled(),
        )
        .await
        .get(KEY)
        .await
        .expect_err("a handshake that never completes must time the call out");
        let elapsed = started.elapsed();

        drop(stop);
        silent.join().expect("the silent listener thread ends");
        assert_timed_out_at(limit, elapsed, &error);
    }

    /// A body that stalls mid-transfer fails with a timeout once the read timeout passes, instead
    /// of waiting forever: the server announces a thousand bytes, sends ten and goes quiet.
    #[compio::test]
    async fn a_stalled_body_times_out_after_the_read_timeout() {
        use futures_lite::stream::{self, StreamExt as _};

        assert_no_tokio_runtime();
        let limit = Duration::from_millis(300);
        let server = LoopbackServer::start(|_| {
            let first = stream::once(Ok::<_, std::io::Error>(
                http_kit::utils::Bytes::from_static(b"{\"Item\":{\""),
            ));
            let mut response = Response::new(Body::from_stream(first.chain(stream::pending())));
            response
                .headers_mut()
                .insert(http_kit::header::CONTENT_LENGTH, 1000.into());
            response
        });

        let started = Instant::now();
        let error = kv_with(
            &server,
            TimeoutConfig::builder().read_timeout(limit).build(),
            RetryConfig::disabled(),
        )
        .await
        .get(KEY)
        .await
        .expect_err("a body that stalls must time the call out");

        assert_timed_out_at(limit, started.elapsed(), &error);
    }

    /// A `4xx` is the service's answer, not a transport failure: the SDK reads the error code from
    /// its body and does not retry it.
    #[compio::test]
    async fn a_client_error_reaches_the_caller_with_the_services_code() {
        assert_no_tokio_runtime();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let server = LoopbackServer::start(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            answer(
                StatusCode::BAD_REQUEST,
                &serde_json::json!({
                    "__type": "com.amazonaws.dynamodb.v20120810#ResourceNotFoundException",
                    "message": "Requested resource not found",
                }),
            )
        });

        let error = kv(&server)
            .await
            .get(KEY)
            .await
            .expect_err("a missing table is an error");

        assert!(
            error.to_string().contains("ResourceNotFoundException"),
            "{error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a 4xx is not retried");
    }
}
