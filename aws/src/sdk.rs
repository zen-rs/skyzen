//! The AWS SDK's I/O, on runtimes that need no tokio context.
//!
//! The SDK reaches the outside world through two extension points: an [`HttpClient`] for its
//! requests and an [`AsyncSleep`] for its retry back-off and timeouts. Its own implementations of
//! both (`rt-tokio`, and `default-https-client`'s hyper) need a tokio runtime, which the compio
//! threads the native server runs on do not have.
//!
//! - [`ZenwaveHttpClient`] sends each request through a zenwave [`NativeBackend`]: a `Send + Sync`
//!   handle onto a connection pool that lives on one thread's compio driver, so an SDK future —
//!   which must be `Send` — stays `Send` while every socket stays on that driver.
//! - [`AsyncIoSleep`] waits on an async-io timer, which any executor can poll.
//!
//! [`config_loader`] wires both into aws-config's loader, and [`load_config`] loads the ambient
//! configuration through it — what every `from_env` constructor in this crate and in `skyzen-s3`
//! calls.

use core::{fmt, time::Duration};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

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
use zenwave::{backend::NativeBackend, Transport, TransportBuilder};

use crate::credential_process::CredentialProcessProvider;

/// The AWS SDK's [`HttpClient`], on zenwave [`NativeBackend`]s.
///
/// Clones share the backends and their connection pools. A `4xx` or `5xx` answer is handed to the
/// SDK as the response it is, so the SDK reads the service's error from its body as it does on any
/// client.
///
/// # Timeouts
///
/// The SDK's operation and attempt timeouts run on [`AsyncIoSleep`] and bound every call. The
/// connect and read timeouts in the [`HttpConnectorSettings`] the SDK passes are zenwave's own:
/// each distinct pair gets a [`NativeBackend`] over a [`Transport`] carrying exactly those
/// timeouts, built on first use and kept, so the SDK's clients with equal settings share one pool,
/// as they do on aws-smithy's own hyper client. Each backend runs its own driver thread.
///
/// - The connect timeout bounds one connection attempt: DNS, TCP, a proxy tunnel and TLS. It
///   reaches the SDK as a [`ConnectorError`] timeout.
/// - The read timeout bounds each wait for bytes of the response, head and body. A head that
///   takes longer is a [`ConnectorError`] timeout; a body that stalls fails its read with an
///   [`std::io::ErrorKind::TimedOut`] error wrapping [`zenwave::Error::Timeout`].
#[derive(Clone)]
pub struct ZenwaveHttpClient {
    /// Describes the base transport, proxy and trust roots, each backend's timeouts go on top of.
    transport: Arc<dyn Fn() -> TransportBuilder + Send + Sync>,
    /// One backend per distinct settings. The SDK calls [`HttpClient::http_connector`] from its
    /// own threads, so the map needs a lock; it is held only to look up or build one backend.
    backends: Arc<Mutex<HashMap<Timeouts, NativeBackend>>>,
}

/// The part of [`HttpConnectorSettings`] a backend depends on, which is not `Hash` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Timeouts {
    connect: Option<Duration>,
    read: Option<Duration>,
}

impl ZenwaveHttpClient {
    /// A client whose backends are built on the transport `transport` describes, with the SDK's
    /// timeouts added. `transport` runs once per distinct set of timeouts.
    #[must_use]
    pub fn new(transport: impl Fn() -> TransportBuilder + Send + Sync + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
            backends: Arc::default(),
        }
    }

    /// A client on the system transport — the OS proxy settings and trust store — whose pools
    /// run on zenwave's compio driver threads.
    #[must_use]
    pub fn system() -> Self {
        Self::new(Transport::builder)
    }

    /// The backend for `timeouts`, built on first use. A transport that cannot be built, such as
    /// one whose system trust store cannot be read, is not cached: the error is returned, and the
    /// next connector the SDK asks for tries again.
    fn backend(&self, timeouts: Timeouts) -> Result<NativeBackend, Arc<zenwave::Error>> {
        // The map is never left half-written, so a lock poisoned by an unrelated panic is still
        // a consistent cache.
        let mut backends = self.backends.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(backend) = backends.get(&timeouts) {
            return Ok(backend.clone());
        }
        let mut transport = (self.transport)();
        if let Some(connect) = timeouts.connect {
            transport = transport.connect_timeout(connect);
        }
        if let Some(read) = timeouts.read {
            transport = transport.read_timeout(read);
        }
        // Built under the lock, so two threads asking for the same settings build one backend.
        let backend = NativeBackend::new(transport.build().map_err(Arc::new)?);
        backends.insert(timeouts, backend.clone());
        drop(backends);
        Ok(backend)
    }
}

impl fmt::Debug for ZenwaveHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZenwaveHttpClient")
            .field("backends", &self.backends)
            .finish_non_exhaustive()
    }
}

impl HttpClient for ZenwaveHttpClient {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        let backend = self.backend(Timeouts {
            connect: settings.connect_timeout(),
            read: settings.read_timeout(),
        });
        if let Err(error) = &backend {
            tracing::error!("the AWS SDK's transport cannot be built: {error}");
        }
        SharedHttpConnector::new(ZenwaveConnector { backend })
    }
}

/// An [`HttpConnector`] on the [`NativeBackend`] built for one set of [`HttpConnectorSettings`],
/// or the error that kept it from being built, which every call then fails with.
#[derive(Debug, Clone)]
struct ZenwaveConnector {
    backend: Result<NativeBackend, Arc<zenwave::Error>>,
}

impl HttpConnector for ZenwaveConnector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let mut backend = match &self.backend {
            Ok(backend) => backend.clone(),
            Err(error) => {
                let error = Arc::clone(error);
                return HttpConnectorFuture::ready(Err(ConnectorError::other(
                    Box::new(TransportUnavailable(error)),
                    None,
                )));
            }
        };
        HttpConnectorFuture::new(async move {
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

/// The transport for a connector could not be built, so no request on it was sent.
#[derive(Debug)]
struct TransportUnavailable(Arc<zenwave::Error>);

impl fmt::Display for TransportUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the AWS SDK's transport cannot be built: {}", self.0)
    }
}

impl core::error::Error for TransportUnavailable {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(&*self.0)
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
