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

use core::{
    fmt,
    future::Future as _,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
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
use pin_project_lite::pin_project;
use zenwave::{backend::NativeBackend, Transport};

use crate::credential_process::CredentialProcessProvider;

/// The AWS SDK's [`HttpClient`], on a zenwave [`NativeBackend`].
///
/// Clones share the backend's connection pool. A `4xx` or `5xx` answer is handed to the SDK as the
/// response it is, so the SDK reads the service's error from its body as it does on any client.
///
/// # Timeouts
///
/// The SDK's operation and attempt timeouts run on [`AsyncIoSleep`] and bound every call. The
/// connect and read timeouts in the [`HttpConnectorSettings`] the SDK passes are applied here, on
/// async-io timers, though zenwave's native backend exposes no per-socket timeout: the response
/// head must arrive within the connect timeout plus the read timeout (one alone bounds it by
/// itself), and then no body chunk may take longer than the read timeout.
#[derive(Clone)]
pub struct ZenwaveHttpClient {
    backend: NativeBackend,
}

impl ZenwaveHttpClient {
    /// A client sending its requests through `backend`.
    #[must_use]
    pub const fn new(backend: NativeBackend) -> Self {
        Self { backend }
    }

    /// A client on the system transport — the OS proxy settings and trust store — whose pool
    /// runs on zenwave's own compio driver thread, shared with every other user of that
    /// transport.
    ///
    /// # Panics
    ///
    /// If zenwave cannot start its driver thread.
    #[must_use]
    pub fn system() -> Self {
        Self::new(NativeBackend::new(Transport::system()))
    }
}

impl fmt::Debug for ZenwaveHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZenwaveHttpClient")
            .field("backend", &self.backend)
            .finish()
    }
}

impl HttpClient for ZenwaveHttpClient {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(ZenwaveConnector {
            backend: self.backend.clone(),
            head_timeout: head_timeout(settings),
            read_timeout: settings.read_timeout(),
        })
    }
}

/// How long a response head may take: the connect timeout plus the read timeout, or whichever of
/// the two is set. `None` when neither is.
fn head_timeout(settings: &HttpConnectorSettings) -> Option<Duration> {
    match (settings.connect_timeout(), settings.read_timeout()) {
        (Some(connect), Some(read)) => Some(connect.saturating_add(read)),
        (connect, read) => connect.or(read),
    }
}

/// An [`HttpConnector`] bound to one set of [`HttpConnectorSettings`].
#[derive(Debug, Clone)]
struct ZenwaveConnector {
    backend: NativeBackend,
    /// The most the response head may take, connecting included.
    head_timeout: Option<Duration>,
    /// The most a wait for the next body chunk may take.
    read_timeout: Option<Duration>,
}

impl HttpConnector for ZenwaveConnector {
    fn call(&self, request: HttpRequest) -> HttpConnectorFuture {
        let mut backend = self.backend.clone();
        let head_timeout = self.head_timeout;
        let read_timeout = self.read_timeout;
        HttpConnectorFuture::new(async move {
            let mut request = request
                .try_into_http1x()
                .map_err(|error| ConnectorError::user(error.into()))?
                .map(Body::new);
            let answer = async { backend.respond(&mut request).await.map_err(connector_error) };
            let response = match head_timeout {
                Some(limit) => {
                    futures_lite::future::or(answer, async {
                        async_io::Timer::after(limit).await;
                        Err(connector_error(zenwave::Error::Timeout))
                    })
                    .await?
                }
                None => answer.await?,
            };
            HttpResponse::try_from(response.map(|body| {
                SdkBody::from_body_1_x(ReadTimeout {
                    body,
                    limit: read_timeout,
                    timer: None,
                })
            }))
            .map_err(|error| ConnectorError::other(error.into(), None))
        })
    }
}

pin_project! {
    /// A response body whose wait for each frame is bounded by `limit`.
    struct ReadTimeout<B> {
        #[pin]
        body: B,
        limit: Option<Duration>,
        // The running timer of the frame being waited for.
        timer: Option<async_io::Timer>,
    }
}

impl<B> http_body::Body for ReadTimeout<B>
where
    B: http_body::Body,
    B::Error: Into<Box<dyn core::error::Error + Send + Sync>>,
{
    type Data = B::Data;
    type Error = Box<dyn core::error::Error + Send + Sync>;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        if let Poll::Ready(frame) = this.body.poll_frame(cx) {
            *this.timer = None;
            return Poll::Ready(frame.map(|frame| frame.map_err(Into::into)));
        }
        if let Some(limit) = *this.limit {
            let timer = this
                .timer
                .get_or_insert_with(|| async_io::Timer::after(limit));
            if Pin::new(timer).poll(cx).is_ready() {
                *this.timer = None;
                return Poll::Ready(Some(Err(Box::new(zenwave::Error::Timeout))));
            }
        }
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
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
            .endpoint_url(format!("http://{}", server.addr()))
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

    /// A server that stalls for `delay` before answering, read by a client configured with
    /// `timeouts`: the call fails as a timeout, within the limit plus slack and well before the
    /// answer would have come.
    async fn assert_stalled_call_times_out(timeouts: TimeoutConfig, limit: Duration) {
        const DELAY: Duration = Duration::from_secs(5);
        assert_no_tokio_runtime();
        let server = LoopbackServer::start(|_| {
            std::thread::sleep(DELAY);
            answer(StatusCode::OK, &serde_json::json!({}))
        });

        let started = Instant::now();
        let error = kv_with(&server, timeouts, RetryConfig::disabled())
            .await
            .get(KEY)
            .await
            .expect_err("a stalled server must time the call out");

        let elapsed = started.elapsed();
        assert!(elapsed >= limit, "failed after {elapsed:?}: {error}");
        assert!(
            elapsed < DELAY,
            "waited out the stall ({elapsed:?}): {error}"
        );
        assert!(
            format!("{error:?}").to_lowercase().contains("timeout"),
            "{error:?}"
        );
    }

    /// The read timeout set on the SDK's configuration reaches the connector and bounds the wait
    /// for the response.
    #[compio::test]
    async fn a_configured_read_timeout_bounds_a_stalled_response() {
        let limit = Duration::from_millis(300);
        // The connect timeout is set too, since aws-config defaults it to seconds and the head's
        // bound is the two together.
        assert_stalled_call_times_out(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_millis(100))
                .read_timeout(limit)
                .build(),
            limit,
        )
        .await;
    }

    /// The connect timeout alone bounds the response head too: zenwave has no separate connect
    /// phase to apply it to.
    #[compio::test]
    async fn a_configured_connect_timeout_bounds_a_stalled_response() {
        let limit = Duration::from_millis(300);
        assert_stalled_call_times_out(
            TimeoutConfig::builder().connect_timeout(limit).build(),
            limit,
        )
        .await;
    }

    /// A body that never produces a frame.
    struct Stalled;

    impl http_body::Body for Stalled {
        type Data = http_kit::utils::Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            std::task::Poll::Pending
        }
    }

    /// A body that stalls mid-transfer fails with a timeout once the read timeout passes, instead
    /// of waiting forever.
    #[compio::test]
    async fn a_stalled_body_times_out_after_the_read_timeout() {
        use std::pin::pin;

        let limit = Duration::from_millis(200);
        let mut body = pin!(super::ReadTimeout {
            body: Stalled,
            limit: Some(limit),
            timer: None,
        });

        let started = Instant::now();
        let frame =
            futures_lite::future::poll_fn(|cx| http_body::Body::poll_frame(body.as_mut(), cx))
                .await
                .expect("the stall ends in an error, not the end of the body");

        assert!(started.elapsed() >= limit);
        let error = frame.expect_err("a stalled body is an error");
        assert!(
            matches!(
                error.downcast_ref::<zenwave::Error>(),
                Some(zenwave::Error::Timeout)
            ),
            "{error}"
        );
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
