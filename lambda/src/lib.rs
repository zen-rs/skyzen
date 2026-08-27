//! AWS Lambda adapter for [Skyzen](https://docs.rs/skyzen).
//!
//! The same `#[skyzen::main]` binary that serves HTTP natively and compiles to a Cloudflare Worker
//! also runs as a Lambda function: `skyzen`'s runtime notices `AWS_LAMBDA_RUNTIME_API` in the
//! environment and hands over to [`run`] instead of binding a listener. Nothing in an application
//! is annotated for Lambda, and nothing about it changes.
//!
//! # What one invocation can be
//!
//! Lambda multiplexes every event source onto one entry point, so the adapter looks at the payload
//! before deciding what it is:
//!
//! - **HTTP** — Function URLs, API Gateway (REST and HTTP APIs), ALB and VPC Lattice. Each shape is
//!   normalized into an `http::Request` by [`lambda_http`], answered by the application's endpoint,
//!   and converted back into the response shape that caller expects.
//! - **SQS** — a payload whose `Records` carry `eventSource: "aws:sqs"` is decoded into the
//!   portable `QueueBatch` and driven through the application's `#[skyzen::queue]` handler. The
//!   reply is a [partial batch response], so only the messages the handler failed are redelivered.
//!
//! An event that is neither is refused by name rather than guessed at.
//!
//! [partial batch response]: https://docs.aws.amazon.com/lambda/latest/dg/services-sqs-errorhandling.html
//!
//! # The runtime is this crate's
//!
//! `lambda_runtime` is built on Tokio, so this crate owns a Tokio runtime and builds the
//! application inside it — Skyzen's own smol-based machinery is bypassed entirely on this path.
//! That is also why [`run`] takes a factory rather than a built endpoint: an AWS SDK client
//! constructed outside a Tokio reactor fails at its first request.
//!
//! # What is deliberately unavailable
//!
//! `WorkerContext` is *not* attached to a Lambda request. Lambda freezes the execution environment
//! the moment a response is returned, so post-response work would not run to completion; a handler
//! that asks for the context is told it is unavailable rather than left with silently frozen work.
//!
//! # Using it
//!
//! An application does not call this crate. It enables the feature:
//!
//! ```toml
//! [dependencies]
//! skyzen = { version = "0.1", features = ["lambda"] }
//! ```
//!
//! and keeps the entry point it already had — `#[skyzen::main]` hands over on its own. What that
//! hand-over amounts to is this, for an application embedding Skyzen by hand:
//!
//! ```no_run
//! use core::convert::Infallible;
//! use http_kit::{Body, Endpoint, Request, Response};
//!
//! #[derive(Clone)]
//! struct Hello;
//!
//! impl Endpoint for Hello {
//!     type Error = Infallible;
//!
//!     async fn respond(&mut self, _request: &mut Request) -> Result<Response, Infallible> {
//!         Ok(Response::new(Body::from_bytes("hello from Lambda")))
//!     }
//! }
//!
//! # fn main() -> Result<(), skyzen_lambda::Error> {
//! // `NoQueueHandler` is what an application with no `#[skyzen::queue]` handler passes: an SQS
//! // event that reaches it fails the invocation rather than acknowledging messages nothing read.
//! skyzen_lambda::run(|| async { (Hello, skyzen_lambda::NoQueueHandler) })
//! # }
//! ```

mod convert;
mod dispatch;
pub mod sqs;

pub use dispatch::{NoQueueHandler, QueueDispatch};
pub use lambda_runtime::Error;

use core::{
    future::Future,
    task::{Context, Poll},
};
use std::{pin::Pin, sync::Arc};

use aws_lambda_events::sqs::SqsEvent;
use futures_util::FutureExt as _;
use http_kit::Endpoint;
use lambda_http::{
    request::LambdaRequest, Adapter, LambdaEvent, Request as LambdaHttpRequest, Response, Service,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use skyzen_services::queue::QueueBatchDisposition;
use tracing::{debug, error, info};

/// Serve `endpoint` and `dispatch` — the two halves `#[skyzen::main]` builds — as a Lambda
/// function, until the runtime shuts the environment down.
///
/// `factory` is run inside this crate's Tokio runtime, so the services it constructs are built in
/// the reactor that will later drive them.
///
/// # Errors
///
/// Returns an error when the Tokio runtime cannot be built, or when the Lambda runtime API stops
/// answering. A failed *invocation* is not an error here: it is reported to Lambda and the loop
/// carries on.
///
/// # Panics
///
/// [`lambda_http`] panics when the Lambda environment variables it needs are absent, which cannot
/// happen on the path that reaches this function: `skyzen` only calls it after seeing
/// `AWS_LAMBDA_RUNTIME_API`.
pub fn run<Factory, Fut, E, D>(factory: Factory) -> Result<(), Error>
where
    Factory: FnOnce() -> Fut,
    Fut: Future<Output = (E, D)>,
    E: Endpoint + Clone + Send + Sync + 'static,
    D: QueueDispatch,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let (endpoint, dispatch) = factory().await;
        info!(queue_handler = D::DECLARED, "Skyzen starting on AWS Lambda");
        lambda_runtime::run(Invocation {
            endpoint,
            dispatch: Arc::new(dispatch),
        })
        .await
    })
}

/// The Lambda service: one invocation in, one JSON payload out.
///
/// The payload arrives as a [`RawValue`] rather than a parsed `serde_json::Value` for two reasons:
/// the HTTP shapes are recognized by [`lambda_http`]'s own deserializer, which needs the raw text,
/// and an event that turns out to be neither HTTP nor SQS is never parsed twice.
struct Invocation<E, D> {
    endpoint: E,
    dispatch: Arc<D>,
}

/// Just enough of an event to tell what it is.
#[derive(Deserialize)]
struct EventKind<'a> {
    #[serde(default, rename = "Records", borrow)]
    records: Vec<RecordKind<'a>>,
}

/// Just enough of a record to tell which service produced it.
#[derive(Deserialize)]
struct RecordKind<'a> {
    #[serde(default, rename = "eventSource", borrow)]
    event_source: Option<&'a str>,
}

impl<E, D> Service<LambdaEvent<Box<RawValue>>> for Invocation<E, D>
where
    E: Endpoint + Clone + Send + Sync + 'static,
    D: QueueDispatch,
{
    type Response = serde_json::Value;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, event: LambdaEvent<Box<RawValue>>) -> Self::Future {
        let endpoint = self.endpoint.clone();
        let dispatch = Arc::clone(&self.dispatch);

        Box::pin(async move {
            let (payload, context) = event.into_parts();
            match classify(payload.get())? {
                EventShape::Sqs => {
                    let event: SqsEvent = serde_json::from_str(payload.get())?;
                    let response = handle_sqs::<D>(&dispatch, event).await?;
                    Ok(serde_json::to_value(response)?)
                }
                EventShape::Http => {
                    let request: LambdaRequest = serde_json::from_str(payload.get())?;
                    let mut adapter = Adapter::from(HttpService { endpoint });
                    let response = adapter
                        .call(LambdaEvent::new(request, context))
                        .await
                        // The inner service renders every endpoint error as a response, so there
                        // is no error left for the adapter to propagate.
                        .unwrap_or_else(|error: core::convert::Infallible| match error {});
                    Ok(serde_json::to_value(response)?)
                }
            }
        })
    }
}

/// What an incoming payload turns out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventShape {
    /// One of the HTTP event shapes `lambda_http` normalizes.
    Http,
    /// An SQS batch.
    Sqs,
}

/// Decide what an event is from its `Records`, if it has any.
///
/// An event carrying records from some *other* service is named in the error rather than handed to
/// the HTTP deserializer, whose "this is not an API Gateway event" message would be true but
/// useless.
fn classify(payload: &str) -> Result<EventShape, Error> {
    let kind: EventKind<'_> = serde_json::from_str(payload)?;
    let Some(first) = kind.records.first() else {
        return Ok(EventShape::Http);
    };

    match first.event_source {
        Some(sqs::SQS_EVENT_SOURCE) => Ok(EventShape::Sqs),
        Some(other) => Err(Error::from(format!(
            "this Lambda received a `{other}` event, which Skyzen does not handle; it serves HTTP \
             events and SQS batches"
        ))),
        None => Err(Error::from(
            "this Lambda received an event whose records name no eventSource, so it cannot be \
             dispatched",
        )),
    }
}

/// Drive the application's queue handler for one pushed batch.
///
/// A handler that fails or panics reports every message as failed rather than taking the function
/// down: the batch is redelivered, and the next invocation starts clean. An application with no
/// handler at all is a different thing entirely — no retry can fix it — so that fails the
/// invocation by name.
async fn handle_sqs<D: QueueDispatch>(
    dispatch: &Arc<D>,
    event: SqsEvent,
) -> Result<aws_lambda_events::sqs::SqsBatchResponse, Error> {
    if !D::DECLARED {
        return Err(Error::from(
            "this Lambda received an SQS event, but the application declares no #[skyzen::queue] \
             handler; annotate one, or point the event source mapping at a function that has one",
        ));
    }

    let decoded = sqs::decode(event)?;
    let messages = decoded.batch.len();
    let queue = decoded.batch.queue.clone();
    debug!(
        queue = queue.as_str(),
        messages,
        attempts = decoded.attempts,
        "handling a pushed queue batch"
    );

    let handled = std::panic::AssertUnwindSafe(dispatch.dispatch(decoded.batch))
        .catch_unwind()
        .await;

    let disposition = match handled {
        Ok(Ok(disposition)) => disposition,
        Ok(Err(error)) => {
            error!(
                queue = queue.as_str(),
                messages,
                attempts = decoded.attempts,
                error = %skyzen_core::ErrorChain(error.as_ref()),
                "queue handler failed; the batch will be redelivered"
            );
            return Ok(sqs::retry_all(&decoded.message_ids));
        }
        Err(panic) => {
            error!(
                queue = queue.as_str(),
                messages,
                attempts = decoded.attempts,
                panic = skyzen_core::panic_message(panic.as_ref()),
                "queue handler panicked; the batch will be redelivered"
            );
            return Ok(sqs::retry_all(&decoded.message_ids));
        }
    };

    Ok(settle(&disposition, &decoded.message_ids, &queue))
}

/// Turn the handler's decision into the batch response, logging what it means.
fn settle(
    disposition: &QueueBatchDisposition,
    message_ids: &[String],
    queue: &str,
) -> aws_lambda_events::sqs::SqsBatchResponse {
    let response = sqs::batch_response(disposition, message_ids);
    if !response.batch_item_failures.is_empty() {
        info!(
            queue,
            failed = response.batch_item_failures.len(),
            of = message_ids.len(),
            "reporting partial batch failures; they are redelivered only when the event source \
             mapping enables ReportBatchItemFailures"
        );
    }
    response
}

/// The inner service [`lambda_http`]'s adapter drives: a normalized request in, a response out.
///
/// Its error type is [`Infallible`](core::convert::Infallible) by construction — an endpoint error
/// is rendered into a response here, through the same helpers the native and Worker backends use,
/// so a 4xx keeps its message and a 5xx does not leak one.
#[derive(Debug, Clone)]
struct HttpService<E> {
    endpoint: E,
}

impl<E> Service<LambdaHttpRequest> for HttpService<E>
where
    E: Endpoint + Clone + Send + Sync + 'static,
{
    type Response = Response<lambda_http::Body>;
    type Error = core::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: LambdaHttpRequest) -> Self::Future {
        let mut endpoint = self.endpoint.clone();

        Box::pin(async move {
            let mut request: http_kit::Request = request.map(convert::into_skyzen_body);
            let method = request.method().clone();
            let path = request.uri().path().to_owned();

            let response = match endpoint.respond(&mut request).await {
                Ok(response) => {
                    info!(
                        method = method.as_str(),
                        path = path.as_str(),
                        status = response.status().as_u16(),
                        "request completed"
                    );
                    response
                }
                Err(error) => {
                    skyzen_core::log_endpoint_error(&error, &method, path.as_str());
                    skyzen_core::error_response(&error)
                }
            };

            let (parts, body) = response.into_parts();
            let body = match convert::into_lambda_body(body).await {
                Ok(body) => body,
                Err(error) => {
                    // The response stream failed after the status line was decided. Lambda has no
                    // partial responses, so the client gets the framework's 500 instead of a
                    // truncated body that would look like a complete one.
                    error!(
                        method = method.as_str(),
                        path = path.as_str(),
                        error = %error,
                        "response body failed part way through"
                    );
                    return Ok(body_failure_response().await);
                }
            };

            Ok(Response::from_parts(parts, body))
        })
    }
}

/// The response sent when a response body fails after its headers were decided.
///
/// Rendered through [`skyzen_core::error_response`] like every other 5xx, so it says as little as
/// every other 5xx does.
async fn body_failure_response() -> Response<lambda_http::Body> {
    let (parts, body) = skyzen_core::error_response(&BodyFailed).into_parts();
    let body = convert::into_lambda_body(body)
        .await
        .expect("an in-memory error body always collects");
    Response::from_parts(parts, body)
}

/// The error behind [`body_failure_response`], so the 5xx redaction policy applies to it too.
#[derive(Debug)]
struct BodyFailed;

impl core::fmt::Display for BodyFailed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the response body failed part way through")
    }
}

impl core::error::Error for BodyFailed {}

impl http_kit::HttpError for BodyFailed {
    fn status(&self) -> http_kit::StatusCode {
        http_kit::StatusCode::INTERNAL_SERVER_ERROR
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify, handle_sqs, EventShape, HttpService, Invocation, NoQueueHandler, QueueDispatch,
    };
    use aws_lambda_events::sqs::SqsEvent;
    use core::future::Future;
    use http_kit::{Endpoint, Request, Response, StatusCode};
    use lambda_http::{LambdaEvent, Request as LambdaHttpRequest, Service as _};
    use skyzen_services::{
        queue::{QueueBatch, QueueBatchDisposition, QueueRetry},
        BoxError,
    };
    use std::sync::Arc;

    const PLAIN_SQS: &str = include_str!("../tests/fixtures/sqs-plain.json");
    const FUNCTION_URL: &str = include_str!("../tests/fixtures/apigw-v2-function-url.json");
    const S3: &str = include_str!("../tests/fixtures/s3-put.json");

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime")
            .block_on(future)
    }

    fn sqs_event() -> SqsEvent {
        serde_json::from_str(PLAIN_SQS).expect("the fixture is a valid SQS event")
    }

    /// An endpoint that echoes the request body back, or fails on demand.
    #[derive(Clone)]
    struct Echo {
        fail: Option<StatusCode>,
    }

    http_kit::http_error!(
        /// The failure `Echo` raises when a test asks for one.
        EchoFailed,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the database at 10.0.0.7 refused the connection"
    );

    impl Endpoint for Echo {
        type Error = EchoFailed;

        fn respond(
            &mut self,
            request: &mut Request,
        ) -> impl Future<Output = Result<Response, Self::Error>> + Send {
            let answered = if self.fail.is_some() {
                Err(EchoFailed::new())
            } else {
                let body = request.body_mut().take().expect("the body is available");
                Ok(Response::new(body))
            };
            core::future::ready(answered)
        }
    }

    /// A dispatcher that answers with a canned decision, or panics.
    #[derive(Clone)]
    struct Canned {
        decide: fn() -> Result<QueueBatchDisposition, BoxError>,
    }

    impl QueueDispatch for Canned {
        const DECLARED: bool = true;

        #[allow(clippy::manual_async_fn)]
        fn dispatch(
            &self,
            _batch: QueueBatch<Vec<u8>>,
        ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send {
            // An `async` block, so a panicking decision panics inside the future the caller polls
            // — which is where the runtime's guard is.
            async move { (self.decide)() }
        }
    }

    #[test]
    fn an_http_invocation_answers_with_the_endpoints_response() {
        let request: LambdaHttpRequest = http::Request::builder()
            .method("POST")
            .uri("https://skyzen.test/greet")
            .body(lambda_http::Body::Text("hello".to_owned()))
            .expect("a request");

        let response = block_on(async {
            HttpService {
                endpoint: Echo { fail: None },
            }
            .call(request)
            .await
        })
        .expect("the service never fails");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.body(),
            &lambda_http::Body::Text("hello".to_owned())
        );
    }

    #[test]
    fn an_endpoint_error_is_rendered_the_way_every_other_backend_renders_it() {
        let request: LambdaHttpRequest = http::Request::builder()
            .uri("https://skyzen.test/greet")
            .body(lambda_http::Body::Empty)
            .expect("a request");

        let response = block_on(async {
            HttpService {
                endpoint: Echo {
                    fail: Some(StatusCode::INTERNAL_SERVER_ERROR),
                },
            }
            .call(request)
            .await
        })
        .expect("the service never fails");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let lambda_http::Body::Text(body) = response.body() else {
            panic!("a JSON error body is text");
        };
        // The 5xx redaction policy holds here too: the internal address never reaches the client.
        assert!(body.contains("Internal server error"), "{body}");
        assert!(!body.contains("10.0.0.7"), "{body}");
    }

    #[test]
    fn an_sqs_invocation_acknowledges_the_batch_the_handler_accepted() {
        let dispatch = Arc::new(Canned {
            decide: || Ok(QueueBatchDisposition::ack_all()),
        });

        let response =
            block_on(handle_sqs(&dispatch, sqs_event())).expect("the batch is dispatchable");

        assert!(response.batch_item_failures.is_empty());
    }

    #[test]
    fn a_failing_handler_reports_every_message_rather_than_failing_the_invocation() {
        let dispatch = Arc::new(Canned {
            decide: || Err(BoxError::from("the handler exploded")),
        });

        let response =
            block_on(handle_sqs(&dispatch, sqs_event())).expect("a handler failure is not fatal");

        // Both messages come back, so SQS redelivers the whole batch.
        assert_eq!(response.batch_item_failures.len(), 2);
    }

    #[test]
    fn a_panicking_handler_retries_the_batch_instead_of_poisoning_the_runtime() {
        let dispatch = Arc::new(Canned {
            decide: || panic!("boom"),
        });

        let response = block_on(handle_sqs(&dispatch, sqs_event()))
            .expect("a panicking handler is caught, like the polling driver catches it");

        assert_eq!(response.batch_item_failures.len(), 2);
    }

    #[test]
    fn a_partially_failed_batch_names_only_the_messages_to_redeliver() {
        let dispatch = Arc::new(Canned {
            decide: || {
                Ok(QueueBatchDisposition::PerMessage(vec![
                    skyzen_services::queue::QueueMessageDisposition::Ack,
                    skyzen_services::queue::QueueMessageDisposition::Retry(QueueRetry::new()),
                ]))
            },
        });

        let response = block_on(handle_sqs(&dispatch, sqs_event())).expect("dispatchable");

        assert_eq!(response.batch_item_failures.len(), 1);
        assert_eq!(
            response.batch_item_failures[0].item_identifier,
            "2e1424d4-f796-459a-8184-9c92662be6da"
        );
    }

    /// Drive the whole service the way `lambda_runtime` does: raw payload in, JSON out.
    fn invoke(payload: &str, dispatch: Canned) -> serde_json::Value {
        let event = LambdaEvent::new(
            serde_json::from_str::<Box<serde_json::value::RawValue>>(payload)
                .expect("the fixture is JSON"),
            lambda_http::Context::default(),
        );

        block_on(async {
            Invocation {
                endpoint: Echo { fail: None },
                dispatch: Arc::new(dispatch),
            }
            .call(event)
            .await
        })
        .expect("the invocation is dispatchable")
    }

    #[test]
    fn a_whole_http_invocation_answers_in_the_shape_the_caller_expects() {
        let response = invoke(
            FUNCTION_URL,
            Canned {
                decide: || Ok(QueueBatchDisposition::ack_all()),
            },
        );

        // The API Gateway v2 response shape, produced by `lambda_http` from the endpoint's answer.
        assert_eq!(response["statusCode"], 200);
        assert_eq!(response["body"], "{\"greeting\":\"hello\"}");
        assert_eq!(response["isBase64Encoded"], false);
    }

    #[test]
    fn a_whole_sqs_invocation_answers_with_a_batch_response() {
        let response = invoke(
            PLAIN_SQS,
            Canned {
                decide: || {
                    Ok(QueueBatchDisposition::PerMessage(vec![
                        skyzen_services::queue::QueueMessageDisposition::Ack,
                        skyzen_services::queue::QueueMessageDisposition::Retry(QueueRetry::new()),
                    ]))
                },
            },
        );

        // The partial batch response shape, not an HTTP one: the same entry point, two contracts.
        let failures = response["batchItemFailures"]
            .as_array()
            .expect("a batch response names its failures");
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0]["itemIdentifier"],
            "2e1424d4-f796-459a-8184-9c92662be6da"
        );
    }

    #[test]
    fn an_sqs_event_with_no_declared_handler_fails_the_invocation_by_name() {
        let dispatch = Arc::new(NoQueueHandler);

        let error = block_on(handle_sqs(&dispatch, sqs_event()))
            .expect_err("nothing can process this message");

        assert!(error.to_string().contains("#[skyzen::queue]"), "{error}");
    }

    #[test]
    fn an_sqs_batch_is_recognized_by_its_event_source() {
        assert_eq!(classify(PLAIN_SQS).expect("classified"), EventShape::Sqs);
    }

    #[test]
    fn an_http_event_has_no_records_at_all() {
        assert_eq!(
            classify(FUNCTION_URL).expect("classified"),
            EventShape::Http
        );
    }

    #[test]
    fn an_event_from_another_service_is_refused_by_name() {
        let error = classify(S3).expect_err("s3 is not dispatchable");

        assert!(error.to_string().contains("aws:s3"), "{error}");
    }
}
