//! The Azure Functions custom-handler integration.
//!
//! A Skyzen application deployed to Azure Functions runs as a [custom handler]: the Functions host
//! starts the compiled binary as a web server on `FUNCTIONS_CUSTOMHANDLER_PORT` and sends it every
//! event. Two kinds of event arrive, and they do not look alike:
//!
//! - An **HTTP trigger** is forwarded as an ordinary HTTP request (that is what
//!   `enableForwardingHttpRequest` in the generated `host.json` buys), so the application's own
//!   router answers it with no involvement from this module.
//! - A **queue trigger** arrives as a `POST /{functionName}` carrying the custom handler's JSON
//!   envelope. Nothing in a router would recognize that, so [`mount`] wraps the application's
//!   endpoint in one that intercepts exactly the declared function names and drives the
//!   `#[skyzen::queue]` handler with them.
//!
//! The interception is live *only* under `FUNCTIONS_CUSTOMHANDLER_PORT`: the same binary run
//! locally serves those paths from the router like any other, because nothing else is POSTing
//! Functions envelopes at it.
//!
//! [custom handler]: https://learn.microsoft.com/azure/azure-functions/functions-custom-handlers

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use http_kit::{Body, Endpoint, Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use skyzen_services::queue::{
    QueueBatch, QueueBatchDisposition, QueueMessage, QueueMessageDisposition,
};
use tracing::{debug, error, info, warn};

use crate::{
    routing::{MethodFilter, ServedRoutes},
    runtime::consumer::ConsumerSet,
};

/// The environment variable the Functions host sets, and the signal that we are running under it.
pub const CUSTOM_HANDLER_PORT_ENV: &str = "FUNCTIONS_CUSTOMHANDLER_PORT";

/// The envelope prefix marking a base64-encoded body.
///
/// Azure Storage queues carry a message as text inside an XML document and offer no property
/// channel to tag an encoding with, so `skyzen-azure` puts the tag in the body itself. The format
/// is owned by `azure/src/storage_queue.rs`; this reverses it for a message the *host* delivered
/// rather than one this process received, which is why it is spelled out again here instead of
/// being called there — the platform crates deliberately do not depend on the framework.
const BASE64_PREFIX: &str = "skyzen-b64:";

/// The envelope prefix marking text that would otherwise be mistaken for an envelope.
const UTF8_PREFIX: &str = "skyzen-utf8:";

/// One `[[azure.queue_triggers]]` entry, as the runtime sees it.
///
/// `#[skyzen::main]` reads these out of `Skyzen.toml` at compile time, so the set of function
/// names this process answers is fixed before it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueTrigger {
    /// The Function name, which is the path the host POSTs the message to.
    pub function: &'static str,
    /// The Storage queue behind it, reported to the handler as [`QueueBatch::queue`].
    pub queue: &'static str,
}

/// Why an application cannot be served under the Functions host.
///
/// Every variant is a wiring mistake that no request will fix, so the runtime refuses to start
/// rather than answering some triggers and quietly dropping others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountError {
    /// The manifest declares a queue trigger but the application has no handler for it.
    NoQueueHandler {
        /// The first offending function, named so the error points somewhere.
        function: String,
    },
    /// Two triggers claim the same function name, so one would shadow the other.
    DuplicateFunction {
        /// The name declared twice.
        function: String,
    },
    /// A trigger's path is already served by a route of the application's own.
    RouteCollision {
        /// The function whose path is taken.
        function: String,
        /// The route pattern that takes it.
        route: String,
    },
}

impl core::fmt::Display for MountError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoQueueHandler { function } => write!(
                f,
                "[[azure.queue_triggers]] declares the function `{function}`, but this application \
                 has no #[skyzen::queue] handler to give its messages to"
            ),
            Self::DuplicateFunction { function } => write!(
                f,
                "[[azure.queue_triggers]] declares the function `{function}` twice; every \
                 Functions name is a distinct URL path, so one would shadow the other"
            ),
            Self::RouteCollision { function, route } => write!(
                f,
                "[[azure.queue_triggers]] declares the function `{function}`, whose queue \
                 messages arrive at `/{function}` — which this application already serves as \
                 `{route}`. Rename the function, or the route"
            ),
        }
    }
}

impl std::error::Error for MountError {}

/// Wrap `endpoint` so the declared queue triggers are answered before it sees them.
///
/// # Errors
///
/// [`MountError`] when a trigger has no handler behind it, when two triggers share a name, or when
/// a trigger's path is one the application already serves as a literal route.
pub fn mount<E, C>(
    endpoint: E,
    consumers: C,
    triggers: &'static [QueueTrigger],
) -> Result<FunctionsHost<E, C>, MountError>
where
    E: Endpoint + ServedRoutes + Clone + Send + Sync + 'static,
    C: ConsumerSet,
{
    let mut seen = BTreeMap::new();
    for trigger in triggers {
        if !C::DECLARES_HANDLER {
            return Err(MountError::NoQueueHandler {
                function: trigger.function.to_owned(),
            });
        }
        if seen.insert(trigger.function, trigger.queue).is_some() {
            return Err(MountError::DuplicateFunction {
                function: trigger.function.to_owned(),
            });
        }
        check_route_collision(trigger, endpoint.served_routes())?;
        info!(
            function = trigger.function,
            queue = trigger.queue,
            "serving an Azure Functions queue trigger at POST /{}",
            trigger.function
        );
    }

    Ok(FunctionsHost {
        endpoint,
        consumers: Arc::new(consumers),
        triggers,
    })
}

/// Refuse a trigger whose path the application already serves, and warn when it only might.
///
/// The distinction is what `matchit` would do with the request: a literal route for the same path
/// is shadowed outright and is a mistake worth refusing to start over, whereas a parameterized or
/// catch-all route merely loses one of the many paths it matches — which is sometimes exactly what
/// the author intended, and is never silent.
fn check_route_collision(
    trigger: &QueueTrigger,
    routes: &[(MethodFilter, String)],
) -> Result<(), MountError> {
    let path = format!("/{}", trigger.function);
    let mut table = matchit::Router::new();
    for (_, route) in routes {
        // A path registered for several methods is one path here, and two routes that `matchit`
        // considers the same are the router's own problem, reported when it was built.
        let _ = table.insert(route.as_str(), route.clone());
    }

    let Ok(shadowed) = table.at(&path) else {
        return Ok(());
    };

    if shadowed.value == &path {
        return Err(MountError::RouteCollision {
            function: trigger.function.to_owned(),
            route: shadowed.value.clone(),
        });
    }

    warn!(
        function = trigger.function,
        route = shadowed.value.as_str(),
        "an Azure Functions queue trigger takes precedence over a route that would also match \
         its path; that path no longer reaches the route while running under the Functions host"
    );
    Ok(())
}

/// An application wrapped for the Functions host: queue triggers first, then everything else.
///
/// Cloned once per connection like any other endpoint, which is why the consumer set — built once,
/// and holding the application's one queue handler — sits behind an [`Arc`] rather than being
/// required to clone itself.
#[derive(Debug)]
pub struct FunctionsHost<E, C> {
    endpoint: E,
    consumers: Arc<C>,
    triggers: &'static [QueueTrigger],
}

impl<E: Clone, C> Clone for FunctionsHost<E, C> {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            consumers: Arc::clone(&self.consumers),
            triggers: self.triggers,
        }
    }
}

impl<E, C> FunctionsHost<E, C> {
    /// The trigger this request is for, if it is for one at all.
    ///
    /// Matched on `POST /{function}` exactly: the host uses no other method or shape for a queue
    /// invocation, and matching more loosely would swallow a `GET` the router should answer.
    fn trigger_for(&self, request: &Request) -> Option<&'static QueueTrigger> {
        if request.method() != Method::POST {
            return None;
        }
        let path = request.uri().path().strip_prefix('/')?;
        self.triggers
            .iter()
            .find(|trigger| trigger.function == path)
    }
}

impl<E, C> Endpoint for FunctionsHost<E, C>
where
    E: Endpoint + Clone + Send + Sync + 'static,
    C: ConsumerSet,
{
    type Error = E::Error;

    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let Some(trigger) = self.trigger_for(request) else {
            return self.endpoint.respond(request).await;
        };

        Ok(self.invoke(trigger, request).await)
    }
}

impl<E, C> FunctionsHost<E, C>
where
    E: Endpoint + Clone + Send + Sync + 'static,
    C: ConsumerSet,
{
    /// Drive the queue handler for one invocation and answer the host.
    async fn invoke(&self, trigger: &QueueTrigger, request: &mut Request) -> Response {
        let batch = match read_batch(trigger, request).await {
            Ok(batch) => batch,
            Err(error) => {
                error!(
                    function = trigger.function,
                    queue = trigger.queue,
                    error = %error,
                    "could not read the Functions queue envelope; the message will be redelivered"
                );
                return failed(&error.to_string());
            }
        };

        debug!(
            function = trigger.function,
            queue = trigger.queue,
            "handling a queue trigger invocation"
        );

        match self.consumers.dispatch(batch).await {
            Ok(disposition) if acknowledged(&disposition) => succeeded(),
            Ok(_) => {
                info!(
                    function = trigger.function,
                    queue = trigger.queue,
                    "the queue handler asked for a retry; the host will redeliver the message"
                );
                failed("the handler asked for this message to be retried")
            }
            Err(error) => {
                error!(
                    function = trigger.function,
                    queue = trigger.queue,
                    error = %skyzen_core::ErrorChain(error.as_ref()),
                    "the queue handler failed; the host will redeliver the message"
                );
                failed("the queue handler failed")
            }
        }
    }
}

/// Whether a decision over a one-message batch acknowledges it.
///
/// The host delivers one message per invocation, so a per-message list of any other length is a
/// handler that answered a different batch than the one it was given: retried rather than settled
/// by index, exactly as the polling driver does with the same mismatch.
const fn acknowledged(disposition: &QueueBatchDisposition) -> bool {
    match disposition {
        QueueBatchDisposition::All(decision) => matches!(decision, QueueMessageDisposition::Ack),
        QueueBatchDisposition::PerMessage(decisions) => {
            matches!(decisions.as_slice(), [QueueMessageDisposition::Ack])
        }
    }
}

/// Read one Functions invocation into the portable batch the handler expects.
async fn read_batch(
    trigger: &QueueTrigger,
    request: &mut Request,
) -> Result<QueueBatch<Vec<u8>>, EnvelopeError> {
    let body = request
        .body_mut()
        .take()
        .map_err(|_| EnvelopeError::BodyUnavailable)?
        .into_bytes()
        .await
        .map_err(|error| EnvelopeError::Unreadable(error.to_string()))?;
    let envelope: InvokeRequest = serde_json::from_slice(&body)
        .map_err(|error| EnvelopeError::Malformed(error.to_string()))?;

    envelope.into_batch(trigger)
}

/// The response a successful invocation returns.
///
/// The application declares no output bindings, so the host has nothing to route onward; the
/// envelope is still what it expects to parse.
fn succeeded() -> Response {
    let payload = serde_json::to_vec(&InvokeResponse::default())
        .expect("an empty invocation response always serializes");
    let mut response = Response::new(Body::from(payload));
    response.headers_mut().insert(
        http_kit::header::CONTENT_TYPE,
        http_kit::header::HeaderValue::from_static("application/json"),
    );
    response
}

/// The response that tells the host this invocation failed, so it redelivers the message.
///
/// A non-2xx status is how a custom handler reports failure; the body carries the reason into the
/// invocation logs (and Application Insights) rather than back to any client, because there is no
/// client — the host is the only caller.
fn failed(reason: &str) -> Response {
    let payload = serde_json::to_vec(&InvokeResponse {
        logs: vec![format!("skyzen: {reason}")],
        ..InvokeResponse::default()
    })
    .expect("an invocation response always serializes");
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        http_kit::header::CONTENT_TYPE,
        http_kit::header::HeaderValue::from_static("application/json"),
    );
    response
}

/// What can go wrong reading an invocation the host sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The request body was already taken, which only a middleware could have done.
    BodyUnavailable,
    /// The body could not be read to the end.
    Unreadable(String),
    /// The body is not the custom handler's JSON envelope.
    Malformed(String),
    /// `Data` does not hold exactly the one trigger binding the generated `function.json` declares.
    UnexpectedBindings(Vec<String>),
    /// The message text carries this framework's base64 tag but is not base64.
    MalformedBase64(String),
}

impl core::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BodyUnavailable => write!(
                f,
                "the invocation body had already been consumed before the trigger could read it"
            ),
            Self::Unreadable(error) => write!(f, "the invocation body could not be read: {error}"),
            Self::Malformed(error) => write!(
                f,
                "the invocation body is not an Azure Functions custom handler envelope: {error}"
            ),
            Self::UnexpectedBindings(names) => write!(
                f,
                "the invocation carried {} input bindings ({}); a Skyzen queue trigger declares \
                 exactly one",
                names.len(),
                names.join(", ")
            ),
            Self::MalformedBase64(error) => write!(
                f,
                "the message is tagged `{BASE64_PREFIX}` but is not valid base64: {error}"
            ),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// The [custom handler request payload].
///
/// [custom handler request payload]: https://learn.microsoft.com/azure/azure-functions/functions-custom-handlers#request-payload
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InvokeRequest {
    /// Keyed by the binding names in `function.json`; a queue trigger has exactly one.
    #[serde(default)]
    data: BTreeMap<String, serde_json::Value>,
    /// Trigger metadata: `DequeueCount`, `Id`, `PopReceipt` and friends.
    #[serde(default)]
    metadata: BTreeMap<String, serde_json::Value>,
}

impl InvokeRequest {
    /// Turn one invocation into the one-message batch the handler is given.
    fn into_batch(self, trigger: &QueueTrigger) -> Result<QueueBatch<Vec<u8>>, EnvelopeError> {
        if self.data.len() != 1 {
            return Err(EnvelopeError::UnexpectedBindings(
                self.data.keys().cloned().collect(),
            ));
        }
        let (_binding, value) = self
            .data
            .into_iter()
            .next()
            .expect("the map holds exactly one entry");

        let attempts = self
            .metadata
            .get("DequeueCount")
            .and_then(count_of)
            .unwrap_or_default();
        let id = self
            .metadata
            .get("Id")
            .and_then(|id| id.as_str().map(ToOwned::to_owned))
            .unwrap_or_default();

        debug!(
            queue = trigger.queue,
            attempts, "decoded a Functions queue invocation"
        );

        Ok(QueueBatch {
            queue: trigger.queue.to_owned(),
            messages: vec![QueueMessage {
                id,
                // The host reports `InsertionTime` as an ISO-8601 string; parsing it would cost a
                // date library for a field the polling driver already fills with a receive time,
                // so this one is a receive time too.
                timestamp_ms: now_ms(),
                body: decode_message(&value)?,
            }],
        })
    }
}

/// Reverse the in-band envelope `AzureStorageQueue::send` applies.
///
/// The host hands over whatever text the queue holds — `messageEncoding: none` in the generated
/// `host.json` is what keeps it from base64-decoding first — so what arrives is exactly what the
/// producer wrote.
fn decode_message(value: &serde_json::Value) -> Result<Vec<u8>, EnvelopeError> {
    // A message the host recognized as JSON arrives as JSON rather than as a string, and the
    // bytes the producer sent are then its serialization.
    let Some(text) = value.as_str() else {
        return Ok(serde_json::to_vec(value)
            .expect("a value that was just deserialized always serializes"));
    };

    if let Some(encoded) = text.strip_prefix(BASE64_PREFIX) {
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| EnvelopeError::MalformedBase64(error.to_string()));
    }
    if let Some(escaped) = text.strip_prefix(UTF8_PREFIX) {
        return Ok(escaped.as_bytes().to_vec());
    }
    Ok(text.as_bytes().to_vec())
}

/// A metadata count, which the host writes as a number and some hosts as a string.
fn count_of(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|count| count.parse().ok()))
}

/// Now, in epoch milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| {
            i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The [custom handler response payload].
///
/// [custom handler response payload]: https://learn.microsoft.com/azure/azure-functions/functions-custom-handlers#response-payload
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
struct InvokeResponse {
    /// Output binding values. A Skyzen queue trigger declares none.
    outputs: BTreeMap<String, serde_json::Value>,
    /// Lines to write to the invocation log.
    logs: Vec<String>,
    /// The `$return` binding's value. A Skyzen queue trigger declares none.
    return_value: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::{
        check_route_collision, failed, mount, succeeded, EnvelopeError, InvokeRequest, MountError,
        QueueTrigger,
    };
    use crate::{
        routing::{MethodFilter, ServedRoutes},
        runtime::consumer::{QueueConsumer, QueueConsumers},
        Endpoint, Method, Request, Response, StatusCode,
    };
    use core::future::Future;
    use skyzen_services::{
        queue::{QueueBatch, QueueBatchDisposition},
        BoxError,
    };

    const INVOCATION: &str = include_str!("../../tests/fixtures/functions-queue-invocation.json");
    const BASE64_INVOCATION: &str =
        include_str!("../../tests/fixtures/functions-queue-invocation-base64.json");

    const TRIGGER: QueueTrigger = QueueTrigger {
        function: "process",
        queue: "jobs",
    };

    /// An endpoint that serves exactly the routes a test names.
    #[derive(Clone, Debug)]
    struct Routes(Vec<(MethodFilter, String)>);

    impl Routes {
        fn new(paths: &[&str]) -> Self {
            Self(
                paths
                    .iter()
                    .map(|path| (MethodFilter::Exact(Method::GET), (*path).to_owned()))
                    .collect(),
            )
        }
    }

    impl ServedRoutes for Routes {
        fn served_routes(&self) -> &[(MethodFilter, String)] {
            &self.0
        }
    }

    impl Endpoint for Routes {
        type Error = core::convert::Infallible;

        fn respond(
            &mut self,
            _request: &mut Request,
        ) -> impl core::future::Future<Output = Result<Response, Self::Error>> + Send {
            core::future::ready(Ok(Response::new(crate::Body::from_bytes("router"))))
        }
    }

    /// A queue handler that accepts everything, so a mount can succeed.
    #[derive(Clone)]
    struct Accepting;

    impl QueueConsumer for Accepting {
        fn handle(
            &self,
            _batch: QueueBatch<Vec<u8>>,
        ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send {
            core::future::ready(Ok(QueueBatchDisposition::ack_all()))
        }
    }

    fn consumers() -> QueueConsumers<Accepting> {
        QueueConsumers::new(Accepting, Vec::new())
    }

    fn envelope(fixture: &str) -> InvokeRequest {
        serde_json::from_str(fixture).expect("the fixture is a custom handler envelope")
    }

    #[test]
    fn a_queue_invocation_decodes_into_a_one_message_batch() {
        let batch = envelope(INVOCATION)
            .into_batch(&TRIGGER)
            .expect("the fixture decodes");

        assert_eq!(batch.queue, "jobs");
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].body, b"{ \"job\": \"resize\" }");
        assert_eq!(batch.messages[0].id, "800ae4b3-bdd2-4c08-badd-f08e5a34b865");
    }

    #[test]
    fn the_storage_queue_envelope_is_reversed_the_way_the_producer_wrote_it() {
        let batch = envelope(BASE64_INVOCATION)
            .into_batch(&TRIGGER)
            .expect("the fixture decodes");

        assert_eq!(batch.messages[0].body, vec![0x00, 0x01, 0xff]);
    }

    #[test]
    fn an_invocation_carrying_more_than_the_trigger_binding_is_refused() {
        let envelope: InvokeRequest =
            serde_json::from_str(r#"{"Data":{"message":"one","extra":"two"},"Metadata":{}}"#)
                .expect("valid JSON");

        let error = envelope
            .into_batch(&TRIGGER)
            .expect_err("a Skyzen queue trigger declares exactly one binding");

        assert!(matches!(error, EnvelopeError::UnexpectedBindings(_)));
        assert!(error.to_string().contains("extra"), "{error}");
    }

    #[test]
    fn a_message_the_host_parsed_as_json_keeps_its_bytes() {
        let envelope: InvokeRequest =
            serde_json::from_str(r#"{"Data":{"message":{"job":"resize"}},"Metadata":{}}"#)
                .expect("valid JSON");

        let batch = envelope.into_batch(&TRIGGER).expect("decodes");

        assert_eq!(batch.messages[0].body, br#"{"job":"resize"}"#);
    }

    #[test]
    fn a_successful_invocation_answers_with_the_hosts_own_envelope() {
        let response = succeeded();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(http_kit::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn a_failed_invocation_answers_with_a_status_the_host_retries() {
        let response = failed("the queue handler failed");

        // A non-2xx is how a custom handler reports failure; the host then redelivers the message.
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn a_trigger_that_shadows_a_literal_route_refuses_to_mount() {
        let error = mount(
            Routes::new(&["/process", "/health"]),
            consumers(),
            &[TRIGGER],
        )
        .expect_err("the route and the trigger cannot both own /process");

        assert!(matches!(error, MountError::RouteCollision { .. }));
        let rendered = error.to_string();
        assert!(rendered.contains("process"), "{rendered}");
    }

    #[test]
    fn a_trigger_beside_unrelated_routes_mounts_cleanly() {
        mount(
            Routes::new(&["/health", "/jobs/{id}"]),
            consumers(),
            &[TRIGGER],
        )
        .expect("nothing collides");
    }

    #[test]
    fn a_parameterized_route_that_would_also_match_is_a_warning_not_a_refusal() {
        // `/{name}` still serves every other name; only this one path is taken over.
        check_route_collision(
            &TRIGGER,
            &[(MethodFilter::Exact(Method::GET), "/{name}".to_owned())],
        )
        .expect("a partial overlap is not a refusal");
    }

    #[test]
    fn a_trigger_declared_twice_refuses_to_mount() {
        let error = mount(Routes::new(&[]), consumers(), &[TRIGGER, TRIGGER])
            .expect_err("two functions cannot share a name");

        assert!(matches!(error, MountError::DuplicateFunction { .. }));
    }

    #[test]
    fn a_trigger_with_no_queue_handler_refuses_to_mount() {
        let error = mount(Routes::new(&[]), (), &[TRIGGER])
            .expect_err("there is nothing to hand the message to");

        assert!(matches!(error, MountError::NoQueueHandler { .. }));
        assert!(
            error.to_string().contains("#[skyzen::queue]"),
            "the error should say what to add"
        );
    }

    #[test]
    fn an_application_with_no_triggers_mounts_and_changes_nothing() {
        mount(Routes::new(&["/health"]), (), &[]).expect("nothing to mount");
    }

    /// Send one request through a mounted application and read back what answered it.
    fn respond(method: Method, path: &str, body: &'static str) -> (StatusCode, String) {
        let mut host = mount(Routes::new(&["/health"]), consumers(), &[TRIGGER])
            .expect("nothing collides with `process`");
        let mut request: Request = http::Request::builder()
            .method(method)
            .uri(path)
            .body(crate::Body::from_bytes(body))
            .expect("a request");

        async_io::block_on(async {
            let response = host.respond(&mut request).await.expect("the host answers");
            let status = response.status();
            let body = response
                .into_body()
                .into_bytes()
                .await
                .expect("the response body collects");
            (status, String::from_utf8(body.to_vec()).expect("utf-8"))
        })
    }

    #[test]
    fn a_posted_invocation_at_the_trigger_path_reaches_the_queue_handler() {
        let (status, body) = respond(Method::POST, "/process", INVOCATION);

        assert_eq!(status, StatusCode::OK);
        // The custom handler envelope, not the router's answer.
        assert!(body.contains("Outputs"), "{body}");
        assert!(!body.contains("router"), "{body}");
    }

    #[test]
    fn another_method_at_the_trigger_path_is_the_routers_to_answer() {
        // The host only ever POSTs an invocation, so anything else is an ordinary request.
        let (status, body) = respond(Method::GET, "/process", "");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "router");
    }

    #[test]
    fn a_post_to_any_other_path_is_the_routers_to_answer() {
        let (status, body) = respond(Method::POST, "/health", "");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "router");
    }

    #[test]
    fn an_invocation_the_handler_cannot_be_given_tells_the_host_to_redeliver() {
        let (status, _) = respond(Method::POST, "/process", "not an envelope");

        // A non-2xx is the only way a custom handler reports failure.
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
