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
//! - A **timer trigger** arrives through the same `POST /{functionName}` envelope shape, its
//!   `Data` carrying the `TimerInfo` the `timerTrigger` binding declared. [`mount`] intercepts
//!   it the same way and drives the `#[skyzen::scheduled]` handler.
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

use http_kit::{Body, Endpoint, Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use skyzen_services::{
    queue::{envelope, QueueBatch, QueueBatchDisposition, QueueMessage, QueueMessageDisposition},
    ScheduledTick,
};
use tracing::{debug, error, info};

use crate::{
    routing::ServedRoutes,
    runtime::{consumer::ConsumerSet, scheduler::ScheduleSet},
};

/// The environment variable the Functions host sets, and the signal that we are running under it.
pub const CUSTOM_HANDLER_PORT_ENV: &str = "FUNCTIONS_CUSTOMHANDLER_PORT";

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

/// One `[[schedule]]` entry, as the Azure Functions host drives it.
///
/// Every declared schedule is a `timerTrigger` function of its own: `function` is both the
/// Functions name and the path the host POSTs its ticks to, and `cron` is the portable
/// expression the manifest wrote — reported to the handler as [`ScheduledTick::cron`] so it
/// reads the same on every target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledTrigger {
    /// The Function name, which is the path the host POSTs the tick to.
    pub function: &'static str,
    /// The portable 5-field cron expression, reported to the handler as [`ScheduledTick::cron`].
    pub cron: &'static str,
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
    /// The manifest declares a `[[schedule]]` but the application has no
    /// `#[skyzen::scheduled]` handler for it.
    NoScheduledHandler {
        /// The first offending function, named so the error points somewhere.
        function: String,
    },
    /// Two triggers claim the same function name, so one would shadow the other.
    DuplicateFunction {
        /// The name declared twice.
        function: String,
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
            Self::NoScheduledHandler { function } => write!(
                f,
                "[[schedule]] declares `{function}`, but this application has no \
                 #[skyzen::scheduled] handler to give its ticks to"
            ),
            Self::DuplicateFunction { function } => write!(
                f,
                "the manifest declares the function `{function}` twice; every Functions name is \
                 a distinct URL path, so one would shadow the other"
            ),
        }
    }
}

impl std::error::Error for MountError {}

/// Wrap `endpoint` so the declared triggers are answered before it sees them.
///
/// Queue and timer triggers share one table of function names — a Functions app cannot declare
/// two functions with one name — so a name claimed in either list is a [`MountError`], whether
/// it collides with its own kind or the other.
///
/// # Errors
///
/// [`MountError`] when a trigger has no handler behind it, when two triggers share a name, or when
/// a trigger's path is one the application already serves as a literal route.
pub fn mount<E, C, S>(
    endpoint: E,
    consumers: C,
    schedules: S,
    queue_triggers: &'static [QueueTrigger],
    scheduled_triggers: &'static [ScheduledTrigger],
) -> Result<FunctionsHost<E, C, S>, MountError>
where
    E: Endpoint + ServedRoutes + Clone + Send + Sync + 'static,
    C: ConsumerSet,
    S: ScheduleSet,
{
    let mut seen = BTreeMap::new();
    for trigger in queue_triggers {
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
        info!(
            function = trigger.function,
            queue = trigger.queue,
            "serving an Azure Functions queue trigger at POST /{}",
            trigger.function
        );
    }
    for trigger in scheduled_triggers {
        if !S::DECLARES_HANDLER {
            return Err(MountError::NoScheduledHandler {
                function: trigger.function.to_owned(),
            });
        }
        if seen.insert(trigger.function, trigger.cron).is_some() {
            return Err(MountError::DuplicateFunction {
                function: trigger.function.to_owned(),
            });
        }
        info!(
            function = trigger.function,
            cron = trigger.cron,
            "serving an Azure Functions timer trigger at POST /{}",
            trigger.function
        );
    }

    Ok(FunctionsHost {
        endpoint,
        consumers: Arc::new(consumers),
        schedules: Arc::new(schedules),
        queue_triggers,
        scheduled_triggers,
    })
}

/// An application wrapped for the Functions host: declared triggers first, then everything else.
///
/// Cloned once per connection like any other endpoint, which is why the event sets — built once,
/// and holding the application's one handler each — sit behind [`Arc`]s rather than being
/// required to clone themselves.
#[derive(Debug)]
pub struct FunctionsHost<E, C, S> {
    endpoint: E,
    consumers: Arc<C>,
    schedules: Arc<S>,
    queue_triggers: &'static [QueueTrigger],
    scheduled_triggers: &'static [ScheduledTrigger],
}

impl<E: Clone, C, S> Clone for FunctionsHost<E, C, S> {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            consumers: Arc::clone(&self.consumers),
            schedules: Arc::clone(&self.schedules),
            queue_triggers: self.queue_triggers,
            scheduled_triggers: self.scheduled_triggers,
        }
    }
}

/// The kind of trigger a `POST /{function}` is invoking.
enum Trigger {
    /// A Storage queue trigger, dispatched to the `#[skyzen::queue]` handler.
    Queue(&'static QueueTrigger),
    /// A timer trigger, dispatched to the `#[skyzen::scheduled]` handler.
    Scheduled(&'static ScheduledTrigger),
}

impl<E, C, S> FunctionsHost<E, C, S> {
    /// The trigger this request is for, if it is for one at all.
    ///
    /// Matched on `POST /{function}` exactly: the host uses no other method or shape for a queue
    /// or timer invocation, and matching more loosely would swallow a `GET` the router should
    /// answer. Queue names win when both kinds somehow claim one — `mount` has already refused
    /// to build that host, so this arm only orders the ordinary case.
    fn trigger_for(&self, request: &Request) -> Option<Trigger> {
        if request.method() != Method::POST {
            return None;
        }
        let path = request.uri().path().strip_prefix('/')?;
        self.queue_triggers
            .iter()
            .find(|trigger| trigger.function == path)
            .map(Trigger::Queue)
            .or_else(|| {
                self.scheduled_triggers
                    .iter()
                    .find(|trigger| trigger.function == path)
                    .map(Trigger::Scheduled)
            })
    }
}

/// Rebuild the HTTP request the host wrapped in its invocation envelope.
///
/// With forwarding off, the host POSTs `{"Data":{"req":{...}}}` to the catch-all function and
/// the envelope — the whole of `Data` — is host-built, so nothing a client sends can ever
/// surface there. The `req` binding is the serialized `HttpRequest`: `method`, `url`,
/// `headers` and `body`, with header values as a string or a string array. A body that is not
/// an envelope means this handler was reached directly — the forwarding mode or a developer
/// on the host's port — and the request passes through untouched.
async fn forwarded_request(request: &mut Request) -> Option<Request> {
    if request.method() != Method::POST {
        return None;
    }
    let body = request.body_mut().take().ok()?.into_bytes().await.ok()?;
    let Some(req) = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|envelope| envelope.get("Data")?.get("req").cloned())
    else {
        // Not an envelope — the forwarding mode or a developer reached the handler directly,
        // and the body the application reads has to go back before the router sees it.
        *request.body_mut() = Body::from(body.to_vec());
        return None;
    };

    // The host serializes its `HttpRequest` with the .NET property names: `Body`, `Method`,
    // `Url`, `Headers`. The lowercase spellings are read as well so a host version that
    // differs does not silently empty the request.
    fn field<'a>(req: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
        req.get(name)
            .or_else(|| req.get(name.to_ascii_lowercase().as_str()))
    }

    let mut forwarded = Request::new(Body::from(
        field(&req, "Body")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
    ));
    *forwarded.method_mut() = field(&req, "Method")
        .and_then(serde_json::Value::as_str)
        .and_then(|method| Method::from_bytes(method.as_bytes()).ok())
        .unwrap_or(Method::GET);
    if let Some(url) = field(&req, "Url").and_then(serde_json::Value::as_str) {
        // The URL is absolute; `Uri` keeps its path and query, which is all routing reads.
        if let Ok(uri) = url.parse() {
            *forwarded.uri_mut() = uri;
        }
    }
    if let Some(headers) = field(&req, "Headers").and_then(serde_json::Value::as_object) {
        for (name, value) in headers {
            // A header arrives as one string or an array of them; every value is kept.
            let values: Vec<String> = value
                .as_str()
                .map(|value| vec![value.to_owned()])
                .or_else(|| {
                    value.as_array().map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                            .collect()
                    })
                })
                .unwrap_or_default();
            for value in values {
                if let (Ok(name), Ok(value)) = (
                    http_kit::header::HeaderName::from_bytes(name.as_bytes()),
                    http_kit::header::HeaderValue::from_str(&value),
                ) {
                    forwarded.headers_mut().append(name, value);
                }
            }
        }
    }
    Some(forwarded)
}

/// Standard base64, for binary `res.body` payloads.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut triple = [0u8; 3];
        triple[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes([0, triple[0], triple[1], triple[2]]);
        for (i, index) in [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63]
            .into_iter()
            .enumerate()
        {
            if i >= chunk.len() + 1 {
                encoded.push('=');
            } else {
                encoded.push(ALPHABET[index as usize] as char);
            }
        }
    }
    encoded
}

/// Serialize the application's response into the `Outputs.res` the `http` output binding
/// declares — the shape the host turns back into the client's HTTP response.
async fn wrapped_response(response: Response) -> Response {
    let (parts, body) = response.into_parts();
    let status = parts.status.as_u16();
    let bytes = body.into_bytes().await.map_err(|_| {
        error!("a response body could not be collected");
    });
    let Ok(bytes) = bytes else {
        return failed("the application's response body could not be collected");
    };
    // Every `res` value crosses the host as an `ExpandoObject`, which the response writer
    // serializes back to JSON — no JSON shape produces `byte[]`, so a binary body has no
    // representation in the wrapped protocol at all. Base64 keeps the bytes recoverable;
    // an AWS-style `isBase64Encoded` flag the host never reads is left off deliberately.
    let body = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => serde_json::json!(text),
        Err(error) => serde_json::json!(base64(&error.into_bytes())),
    };
    // The host reads `res.headers` as name -> string — arrays come out as their serialized
    // form, not the values. Repeated values join with ", " the way HTTP itself folds them;
    // `Set-Cookie` is the one header that must not fold, and `res.cookies` carries each as
    // its own object instead. `Content-Length` is the host's to compute again: the wrapped
    // body is not the bytes it was counted for.
    let mut headers: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    for (name, value) in &parts.headers {
        if name.as_str().eq_ignore_ascii_case("content-length") {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        if name.as_str().eq_ignore_ascii_case("set-cookie") {
            // `res.cookies` deserializes through `ExpandoObject` — the tuples the host's
            // `RawScriptResult` expects can never arrive that way, so Set-Cookie survives
            // only as a header, and a headers dictionary holds one value per name. The
            // first cookie goes in whole; anything further has no correct representation —
            // a comma join violates RFC 6265 and a renamed header leaks `HttpOnly` values
            // to JavaScript — so it is dropped and the loss is logged.
            if headers.contains_key(name.as_str()) {
                tracing::warn!(
                    "dropping a Set-Cookie header: Azure's wrapped response cannot carry \
                     multiple cookies"
                );
            } else {
                headers.insert(name.as_str().to_owned(), value.into());
            }
            continue;
        }
        match headers.entry(name.as_str().to_owned()) {
            serde_json::map::Entry::Occupied(mut entry) => {
                if let serde_json::Value::String(joined) = entry.get_mut() {
                    joined.push_str(", ");
                    joined.push_str(value);
                }
            }
            serde_json::map::Entry::Vacant(entry) => {
                entry.insert(value.into());
            }
        }
    }
    let payload = serde_json::json!({
        "Outputs": {
            "res": {
                "statusCode": status,
                "headers": headers,
                "body": body,
            }
        },
        "Logs": []
    });
    let mut wrapped = Response::new(Body::from(serde_json::to_vec(&payload).unwrap_or_default()));
    wrapped.headers_mut().insert(
        http_kit::header::CONTENT_TYPE,
        http_kit::header::HeaderValue::from_static("application/json"),
    );
    wrapped
}

/// Whether `body` is a forwarded HTTP request rather than a trigger invocation.
///
/// The host wraps every forwarded request under `Data.req`; the trigger's invocation instead
/// carries its binding under `Data` (`message` for a queue trigger, `timer` for a timer). A
/// body is an invocation only when it carries the trigger's own binding *and* no `req` —
/// anything else is the router's to answer, because a body shaped like neither can only have
/// come from the open HTTP surface, never the host's trigger path.
fn is_invocation(body: &[u8], binding: &str) -> bool {
    let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let Some(data) = envelope.get("Data") else {
        return false;
    };
    data.get("req").is_none() && data.get(binding).is_some()
}

impl Trigger {
    /// The `Data` key an invocation for this trigger always carries — `message` for a queue
    /// trigger, `timer` for a timer trigger.
    fn binding(&self) -> &'static str {
        match self {
            Self::Queue(_) => "message",
            Self::Scheduled(_) => "timer",
        }
    }
}

impl<E, C, S> Endpoint for FunctionsHost<E, C, S>
where
    E: Endpoint + Clone + Send + Sync + 'static,
    C: ConsumerSet,
    S: ScheduleSet,
{
    type Error = E::Error;

    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let Some(trigger) = self.trigger_for(request) else {
            // The envelope protocol only exists when a trigger does — the generated bundle
            // turns off forwarding exactly then, and only then can `Data.req` arrive. With no
            // triggers the host forwards the client's body verbatim, and a client who sends
            // `{"Data":{"req":{...}}}` shaped JSON is just sending a body.
            if self.queue_triggers.is_empty() && self.scheduled_triggers.is_empty() {
                return self.endpoint.respond(request).await;
            }
            // With no forwarding configured the host delivers every HTTP request to the
            // catch-all function wrapped in its invocation envelope, the client request
            // nested under `Data.req`. Unwrap that back into the request it was, and the
            // response back into the `Outputs.res` the `http` output binding expects.
            let Some(forwarded) = forwarded_request(request).await else {
                return self.endpoint.respond(request).await;
            };
            *request = forwarded;
            let response = self.endpoint.respond(request).await?;
            return Ok(wrapped_response(response).await);
        };

        // A path match alone cannot decide what this POST is when the host forwards raw HTTP:
        // `POST /tick` from the outside lands here too, and its body is whatever the client
        // sent. The envelope is the boundary — a real invocation carries the trigger's own
        // binding under `Data`, and a client can only ever land inside `Data.req`, never at
        // the top level. Anything shaped like neither is the router's to answer.
        let Ok(body) = request.body_mut().take() else {
            return self.endpoint.respond(request).await;
        };
        let Ok(body) = body.into_bytes().await else {
            *request.body_mut() = Body::from(Vec::new());
            return self.endpoint.respond(request).await;
        };
        if !is_invocation(&body, trigger.binding()) {
            *request.body_mut() = Body::from(body.to_vec());
            return self.endpoint.respond(request).await;
        }

        Ok(match trigger {
            Trigger::Queue(trigger) => self.invoke_queue(trigger, &body).await,
            Trigger::Scheduled(trigger) => self.invoke_scheduled(trigger, &body).await,
        })
    }
}

impl<E, C, S> FunctionsHost<E, C, S>
where
    E: Endpoint + Clone + Send + Sync + 'static,
    C: ConsumerSet,
    S: ScheduleSet,
{
    /// Drive the queue handler for one invocation and answer the host.
    async fn invoke_queue(&self, trigger: &QueueTrigger, body: &[u8]) -> Response {
        let batch = match read_batch(trigger, body) {
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

    /// Drive the scheduled handler for one invocation and answer the host.
    ///
    /// A timer trigger has no retry of its own to honor: `Err` is only ever reported to the
    /// invocation log, the way an `Err` return reads on Cloudflare. A panic is reported the
    /// same way — the same outcome both the native loop and Lambda give a panicking handler,
    /// so one handler reads identically on all four targets.
    async fn invoke_scheduled(&self, trigger: &ScheduledTrigger, body: &[u8]) -> Response {
        let tick = match read_tick(trigger, body) {
            Ok(tick) => tick,
            Err(error) => {
                error!(
                    function = trigger.function,
                    error = %error,
                    "could not read the Functions timer envelope; the invocation fails"
                );
                return failed(&error.to_string());
            }
        };

        debug!(
            function = trigger.function,
            cron = trigger.cron,
            "handling a timer trigger invocation"
        );

        let handled = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            self.schedules.dispatch(tick),
        ))
        .await;
        match handled {
            Ok(Ok(())) => succeeded(),
            Ok(Err(error)) => {
                error!(
                    function = trigger.function,
                    cron = trigger.cron,
                    error = %skyzen_core::ErrorChain(error.as_ref()),
                    "the scheduled handler failed"
                );
                failed("the scheduled handler failed")
            }
            Err(panic) => {
                error!(
                    function = trigger.function,
                    cron = trigger.cron,
                    panic = skyzen_core::panic_message(panic.as_ref()),
                    "the scheduled handler panicked"
                );
                failed("the scheduled handler panicked")
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
fn read_batch(trigger: &QueueTrigger, body: &[u8]) -> Result<QueueBatch<Vec<u8>>, EnvelopeError> {
    let envelope: InvokeRequest = serde_json::from_slice(body)
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

/// Read one timer invocation into the portable tick the handler expects.
///
/// The `timerTrigger` binding's `Data` carries its `TimerInfo` under the name the generated
/// `function.json` declares — always `timer` for a Skyzen function. [`ScheduledTick::cron`]
/// comes from the manifest rather than the payload: the host reports the NCRONTAB the CLI
/// translated, and the handler should read the portable expression it was written with.
/// [`ScheduledTick::scheduled_time_ms`] is the `ScheduleStatus.Next` the host reports — the
/// due instant this invocation fulfills, which is also what a past-due firing still names —
/// or the receive time when the invocation carries none or fired early (a manual `func`
/// invoke or `runOnStartup`).
fn read_tick(trigger: &ScheduledTrigger, body: &[u8]) -> Result<ScheduledTick, EnvelopeError> {
    let envelope: InvokeRequest = serde_json::from_slice(body)
        .map_err(|error| EnvelopeError::Malformed(error.to_string()))?;

    envelope.into_tick(trigger)
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
    /// The message text is not a [`envelope`] the producer could have written.
    MalformedEnvelope(String),
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
            Self::MalformedEnvelope(error) => {
                write!(f, "the queue message could not be decoded: {error}")
            }
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

    /// Turn one timer invocation into the portable tick the handler is given.
    ///
    /// # Errors
    ///
    /// [`EnvelopeError::UnexpectedBindings`] when `Data` does not hold the `timer` binding — the
    /// only one a timer trigger can carry.
    fn into_tick(self, trigger: &ScheduledTrigger) -> Result<ScheduledTick, EnvelopeError> {
        if self.data.len() != 1 {
            return Err(EnvelopeError::UnexpectedBindings(
                self.data.keys().cloned().collect(),
            ));
        }
        let (binding, value) = self
            .data
            .into_iter()
            .next()
            .expect("the map holds exactly one entry");
        if binding != "timer" {
            return Err(EnvelopeError::UnexpectedBindings(vec![binding]));
        }

        // `ScheduleStatus` reports the occurrences on either side of this invocation:
        // `Last` is the *previous* firing and `Next` is the one being delivered — the due
        // instant this tick names. A `Next` still in the future means the host fired early
        // (`runOnStartup`, a manual invoke), which is no scheduled occurrence at all; receive
        // time reports those the way a pull-based backend does.
        let scheduled_time_ms = value
            .get("ScheduleStatus")
            .and_then(|status| status.get("Next"))
            .and_then(serde_json::Value::as_str)
            .and_then(|next| chrono::DateTime::parse_from_rfc3339(next).ok())
            .map_or_else(now_ms, |next| {
                let due = next.timestamp_millis();
                if due <= now_ms() {
                    due
                } else {
                    now_ms()
                }
            });

        debug!(
            function = trigger.function,
            cron = trigger.cron,
            "decoded a Functions timer invocation"
        );

        Ok(ScheduledTick::new(
            trigger.cron.to_owned(),
            scheduled_time_ms,
        ))
    }
}

/// Reverse the in-band [`envelope`] `AzureStorageQueue::send` applies.
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

    envelope::decode(text).map_err(|error| EnvelopeError::MalformedEnvelope(error.to_string()))
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
    use super::{failed, mount, succeeded, EnvelopeError, InvokeRequest, MountError, QueueTrigger};
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
    fn a_trigger_beside_routes_that_share_its_path_mounts_cleanly() {
        // The host wraps every HTTP request at the catch-all function, so a route's path and a
        // trigger's path never meet: `POST /process` from a client unwraps into the route,
        // while `POST /process` carrying `Data.message` is the trigger invocation.
        mount(
            Routes(vec![(
                MethodFilter::Exact(Method::POST),
                "/process".to_owned(),
            )]),
            consumers(),
            (),
            &[TRIGGER],
            &[],
        )
        .expect("a shared path is not a collision when the host wraps");
    }

    #[test]
    fn a_trigger_declared_twice_refuses_to_mount() {
        let error = mount(Routes::new(&[]), consumers(), (), &[TRIGGER, TRIGGER], &[])
            .expect_err("two functions cannot share a name");

        assert!(matches!(error, MountError::DuplicateFunction { .. }));
    }

    #[test]
    fn a_trigger_with_no_queue_handler_refuses_to_mount() {
        let error = mount(Routes::new(&[]), (), (), &[TRIGGER], &[])
            .expect_err("there is nothing to hand the message to");

        assert!(matches!(error, MountError::NoQueueHandler { .. }));
        assert!(
            error.to_string().contains("#[skyzen::queue]"),
            "the error should say what to add"
        );
    }

    #[test]
    fn an_application_with_no_triggers_mounts_and_changes_nothing() {
        mount(Routes::new(&["/health"]), (), (), &[], &[]).expect("nothing to mount");
    }

    /// Send one request through a mounted application and read back what answered it.
    fn respond(method: Method, path: &str, body: &'static str) -> (StatusCode, String) {
        let mut host = mount(Routes::new(&["/health"]), consumers(), (), &[TRIGGER], &[])
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
    fn a_post_without_the_invocation_envelope_is_the_routers_to_answer() {
        // A body that is neither a real invocation (`Data.message`) nor a forwarded request
        // (`Data.req`) cannot come from the host's trigger path — it reaches the router like
        // every other request and never sees the handler.
        let (status, body) = respond(Method::POST, "/process", "not an envelope");

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "router");
    }
}
