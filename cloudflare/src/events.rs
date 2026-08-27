//! Cloudflare worker event wrappers for queue and scheduled handlers.

use std::future::Future;

use serde::de::DeserializeOwned;
use skyzen::ErrorChain;
use skyzen_services::{
    QueueBatch, QueueBatchDisposition, QueueMessage, QueueMessageDisposition, QueueRetry,
};
use thiserror::Error;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::future_to_promise;
use worker::send::IntoSendFuture;

/// Errors returned by Cloudflare queue and scheduled event wrappers.
#[derive(Debug, Error)]
pub enum CfEventError {
    /// The underlying runtime returned an error.
    #[error("cloudflare event runtime error: {0}")]
    Runtime(String),

    /// Message deserialization failed.
    #[error("cloudflare event serialization error: {0}")]
    Serialization(#[from] serde_wasm_bindgen::Error),

    /// A raw-bytes message body could not be decoded as JSON.
    #[error("cloudflare event decode error: {0}")]
    Decode(String),
}

/// Record a failing event handler before the error is thrown to the Workers runtime.
///
/// The platform only ever sees an unstructured string, so this is the framework's one chance to
/// emit a structured record with the full cause chain — the same treatment HTTP errors get.
fn log_event_error(error: &(dyn std::error::Error + 'static)) -> JsValue {
    tracing::error!(error = %ErrorChain(error), "cloudflare event handler failed");
    JsValue::from_str(&error.to_string())
}

/// Record a failing queue handler, naming the message ids the failure applies to.
///
/// Whether the batch is retried is decided by the platform from the thrown error, so the ids are
/// the only way an operator can tell which messages were affected. They are read back from the
/// batch; if the runtime refuses that lookup the error is still logged without them.
fn log_queue_error(error: &(dyn std::error::Error + 'static), batch: &CfQueueBatch) -> JsValue {
    let queue = batch.queue().ok();
    let ids = batch.messages().ok().map(|messages| {
        messages
            .iter()
            .map(|message| message.id().unwrap_or_else(|_| "<unknown>".to_owned()))
            .collect::<Vec<_>>()
    });
    tracing::error!(
        queue = queue.as_deref().unwrap_or("<unknown>"),
        messages = ?ids,
        error = %ErrorChain(error),
        "cloudflare queue handler failed"
    );
    JsValue::from_str(&error.to_string())
}

/// Convert queue/scheduled handler results into a Cloudflare worker return type.
pub trait IntoWorkerResult {
    /// Convert the handler output to a Cloudflare worker-compatible result.
    ///
    /// # Errors
    ///
    /// Returns the handler's error converted to a `JsValue`.
    fn into_worker_result(self) -> Result<(), JsValue>;
}

impl IntoWorkerResult for () {
    fn into_worker_result(self) -> Result<(), JsValue> {
        Ok(())
    }
}

impl<E> IntoWorkerResult for Result<(), E>
where
    E: std::error::Error + 'static,
{
    fn into_worker_result(self) -> Result<(), JsValue> {
        self.map_err(|error| log_event_error(&error))
    }
}

/// Convert queue handler results into Cloudflare queue acknowledgements/retries.
pub trait IntoQueueWorkerResult {
    /// Convert the handler output to Cloudflare queue operations.
    ///
    /// # Errors
    ///
    /// Returns the handler's error, or an ack/retry failure, as a `JsValue`.
    fn into_queue_worker_result(self, batch: &CfQueueBatch) -> Result<(), JsValue>;
}

impl IntoQueueWorkerResult for () {
    fn into_queue_worker_result(self, batch: &CfQueueBatch) -> Result<(), JsValue> {
        batch
            .ack_all()
            .map_err(|error| log_queue_error(&error, batch))
    }
}

impl<E> IntoQueueWorkerResult for Result<(), E>
where
    E: std::error::Error + 'static,
{
    fn into_queue_worker_result(self, batch: &CfQueueBatch) -> Result<(), JsValue> {
        self.map_err(|error| log_queue_error(&error, batch))?;
        batch
            .ack_all()
            .map_err(|error| log_queue_error(&error, batch))
    }
}

impl IntoQueueWorkerResult for QueueBatchDisposition {
    fn into_queue_worker_result(self, batch: &CfQueueBatch) -> Result<(), JsValue> {
        batch
            .apply_disposition(self)
            .map_err(|error| log_queue_error(&error, batch))
    }
}

impl<E> IntoQueueWorkerResult for Result<QueueBatchDisposition, E>
where
    E: std::error::Error + 'static,
{
    fn into_queue_worker_result(self, batch: &CfQueueBatch) -> Result<(), JsValue> {
        let disposition = self.map_err(|error| log_queue_error(&error, batch))?;
        batch
            .apply_disposition(disposition)
            .map_err(|error| log_queue_error(&error, batch))
    }
}

/// The `ExecutionContext` a queue, email or tail handler receives.
///
/// One type for the three, because the platform hands all three the same `ExecutionContext`
/// object. The `scheduled` handler gets a differently-typed one, wrapped by
/// [`CfScheduleContext`]; the fetch handler's is [`skyzen::runtime::WorkerContext`], which is
/// dual-target because a fetch handler is the one that also runs natively.
#[derive(Debug)]
pub struct CfEventContext {
    inner: worker_sys::Context,
}

impl CfEventContext {
    /// Create from the raw worker context.
    #[must_use]
    pub const fn new(inner: worker_sys::Context) -> Self {
        Self { inner }
    }

    /// Schedule asynchronous work to complete after the handler returns.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the promise.
    pub fn wait_until<F>(&self, future: F) -> Result<(), CfEventError>
    where
        F: Future<Output = ()> + 'static,
    {
        self.inner
            .wait_until(&future_to_promise(async move {
                future.await;
                Ok(JsValue::UNDEFINED)
            }))
            .map_err(js_err)
    }

    /// Tell the runtime to continue the request even if an exception is thrown.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the request.
    pub fn pass_through_on_exception(&self) -> Result<(), CfEventError> {
        self.inner.pass_through_on_exception().map_err(js_err)
    }
}

/// Retry options for queue messages or batches.
#[derive(Debug, Clone, Copy, Default)]
pub struct CfQueueRetryOptions {
    delay_seconds: Option<u32>,
}

impl CfQueueRetryOptions {
    /// Create empty retry options.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            delay_seconds: None,
        }
    }

    /// Delay the retried message by the given number of seconds.
    #[must_use]
    pub const fn with_delay_seconds(mut self, delay_seconds: u32) -> Self {
        self.delay_seconds = Some(delay_seconds);
        self
    }

    fn into_js(self) -> JsValue {
        let object = js_sys::Object::new();
        if let Some(delay_seconds) = self.delay_seconds {
            let _ = js_sys::Reflect::set(
                &object,
                &JsValue::from_str("delaySeconds"),
                &JsValue::from_f64(f64::from(delay_seconds)),
            );
        }
        object.into()
    }
}

/// A single queue message delivered to a consumer.
#[derive(Debug, Clone)]
pub struct CfQueueMessage {
    inner: worker_sys::Message,
}

impl CfQueueMessage {
    /// Create from the raw queue message.
    #[must_use]
    pub const fn new(inner: worker_sys::Message) -> Self {
        Self { inner }
    }

    /// The system-generated message id.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn id(&self) -> Result<String, CfEventError> {
        self.inner.id().map(String::from).map_err(js_err)
    }

    /// The message timestamp in milliseconds since the Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn timestamp_ms(&self) -> Result<i64, CfEventError> {
        let timestamp = self.inner.timestamp().map_err(js_err)?;
        let millis = timestamp.get_time();
        if !millis.is_finite() || millis.fract() != 0.0 {
            return Err(CfEventError::Runtime(format!(
                "queue message timestamp is not an integer millisecond value: {millis}"
            )));
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            Ok(millis as i64)
        }
    }

    /// How many times this message has been delivered, counting this one.
    ///
    /// The platform starts at 1, so a value above 1 means an earlier delivery was retried or left
    /// unacknowledged — which is what a handler checks before giving up on a poison message and
    /// acking it away rather than retrying forever.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup or reports a non-integer count.
    pub fn attempts(&self) -> Result<u32, CfEventError> {
        let value = js_sys::Reflect::get(self.inner.as_ref(), &JsValue::from_str("attempts"))
            .map_err(js_err)?;
        let attempts = value.as_f64().ok_or_else(|| {
            CfEventError::Runtime(format!(
                "queue message `attempts` is not a number: {value:?}"
            ))
        })?;
        if !attempts.is_finite() || attempts.fract() != 0.0 || attempts < 0.0 {
            return Err(CfEventError::Runtime(format!(
                "queue message `attempts` is not a whole non-negative count: {attempts}"
            )));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(attempts as u32)
    }

    /// Access the raw message body as a JS value.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn raw_body(&self) -> Result<JsValue, CfEventError> {
        self.inner.body().map_err(js_err)
    }

    /// Deserialize the message body into `T`.
    ///
    /// Bodies produced with `contentType: "bytes"` (as [`crate::CfQueue`]
    /// sends them) arrive as an `ArrayBuffer` and are decoded with
    /// `serde_json`; any other body is deserialized directly from the JS
    /// value.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup or deserialization fails.
    pub fn body_json<T: DeserializeOwned>(&self) -> Result<T, CfEventError> {
        let raw = self.raw_body()?;
        if raw.is_instance_of::<js_sys::ArrayBuffer>() || raw.is_instance_of::<js_sys::Uint8Array>()
        {
            let bytes = js_sys::Uint8Array::new(&raw).to_vec();
            return serde_json::from_slice(&bytes)
                .map_err(|error| CfEventError::Decode(error.to_string()));
        }
        serde_wasm_bindgen::from_value(raw).map_err(Into::into)
    }

    /// Decode the message into the portable queue message type.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if metadata lookup or body decoding fails.
    pub fn decode_json<T: DeserializeOwned>(&self) -> Result<QueueMessage<T>, CfEventError> {
        Ok(QueueMessage {
            id: self.id()?,
            timestamp_ms: self.timestamp_ms()?,
            body: self.body_json()?,
        })
    }

    /// Mark the message as acknowledged.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the ack.
    pub fn ack(&self) -> Result<(), CfEventError> {
        self.inner.ack().map_err(js_err)
    }

    /// Mark the message for retry.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the retry.
    pub fn retry(&self) -> Result<(), CfEventError> {
        self.inner.retry(JsValue::NULL).map_err(js_err)
    }

    /// Mark the message for retry with options.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the retry.
    pub fn retry_with_options(&self, options: CfQueueRetryOptions) -> Result<(), CfEventError> {
        self.inner.retry(options.into_js()).map_err(js_err)
    }
}

/// A queue batch delivered to a Cloudflare consumer worker.
#[derive(Debug, Clone)]
pub struct CfQueueBatch {
    inner: worker_sys::MessageBatch,
}

impl CfQueueBatch {
    /// Create from the raw queue batch.
    #[must_use]
    pub const fn new(inner: worker_sys::MessageBatch) -> Self {
        Self { inner }
    }

    /// The queue name that produced this batch.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn queue(&self) -> Result<String, CfEventError> {
        self.inner.queue().map(String::from).map_err(js_err)
    }

    /// Return all messages in the batch.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn messages(&self) -> Result<Vec<CfQueueMessage>, CfEventError> {
        let messages = self.inner.messages().map_err(js_err)?;
        let mut output = Vec::with_capacity(messages.length() as usize);
        for value in messages.iter() {
            output.push(CfQueueMessage::new(value.into()));
        }
        Ok(output)
    }

    /// Decode the batch into the portable queue batch type.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if metadata lookup or body decoding fails.
    pub fn decode_json<T: DeserializeOwned>(&self) -> Result<QueueBatch<T>, CfEventError> {
        let queue = self.queue()?;
        let messages = self
            .messages()?
            .iter()
            .map(CfQueueMessage::decode_json)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(QueueBatch { queue, messages })
    }

    /// Mark the whole batch as acknowledged.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the ack.
    pub fn ack_all(&self) -> Result<(), CfEventError> {
        self.inner.ack_all().map_err(js_err)
    }

    /// Mark the whole batch for retry.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the retry.
    pub fn retry_all(&self) -> Result<(), CfEventError> {
        self.inner.retry_all(JsValue::NULL).map_err(js_err)
    }

    /// Mark the whole batch for retry with options.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the retry.
    pub fn retry_all_with_options(&self, options: CfQueueRetryOptions) -> Result<(), CfEventError> {
        self.inner.retry_all(options.into_js()).map_err(js_err)
    }

    /// Apply a portable batch disposition to the underlying Cloudflare queue batch.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects an ack/retry operation or if the
    /// per-message disposition length does not match the batch size.
    pub fn apply_disposition(
        &self,
        disposition: QueueBatchDisposition,
    ) -> Result<(), CfEventError> {
        match disposition {
            QueueBatchDisposition::All(QueueMessageDisposition::Ack) => self.ack_all(),
            QueueBatchDisposition::All(QueueMessageDisposition::Retry(retry)) => {
                self.retry_all_with_options(queue_retry_to_cf(retry))
            }
            QueueBatchDisposition::PerMessage(actions) => {
                let messages = self.messages()?;
                if messages.len() != actions.len() {
                    return Err(CfEventError::Runtime(format!(
                        "queue disposition length mismatch: expected {}, got {}",
                        messages.len(),
                        actions.len()
                    )));
                }

                for (message, action) in messages.iter().zip(actions) {
                    match action {
                        QueueMessageDisposition::Ack => message.ack()?,
                        QueueMessageDisposition::Retry(retry) => {
                            message.retry_with_options(queue_retry_to_cf(retry))?;
                        }
                    }
                }

                Ok(())
            }
        }
    }
}

/// A Cloudflare scheduled event.
#[derive(Debug, Clone)]
pub struct CfScheduledEvent {
    inner: worker_sys::ScheduledEvent,
}

impl CfScheduledEvent {
    /// Create from the raw scheduled event.
    #[must_use]
    pub const fn new(inner: worker_sys::ScheduledEvent) -> Self {
        Self { inner }
    }

    /// The cron expression that triggered this event.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn cron(&self) -> Result<String, CfEventError> {
        self.inner.cron().map_err(js_err)
    }

    /// The scheduled time in milliseconds since the Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn scheduled_time_ms(&self) -> Result<i64, CfEventError> {
        let millis = self.inner.scheduled_time().map_err(js_err)?;
        if !millis.is_finite() || millis.fract() != 0.0 {
            return Err(CfEventError::Runtime(format!(
                "scheduled event time is not an integer millisecond value: {millis}"
            )));
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            Ok(millis as i64)
        }
    }
}

/// A Cloudflare scheduled event context.
#[derive(Debug, Clone)]
pub struct CfScheduleContext {
    inner: worker_sys::ScheduleContext,
}

impl CfScheduleContext {
    /// Create from the raw schedule context.
    #[must_use]
    pub const fn new(inner: worker_sys::ScheduleContext) -> Self {
        Self { inner }
    }

    /// Schedule asynchronous work to complete after the handler returns.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the promise.
    pub fn wait_until<F>(&self, future: F) -> Result<(), CfEventError>
    where
        F: Future<Output = ()> + 'static,
    {
        self.inner
            .wait_until(future_to_promise(async move {
                future.await;
                Ok(JsValue::UNDEFINED)
            }))
            .map_err(js_err)
    }
}

// ── Email Workers ──

/// An inbound email delivered to an `#[skyzen::email]` handler.
///
/// The three things a handler can do with a message are
/// [`forward`](Self::forward) it to a verified destination,
/// [`reject`](Self::reject) it so the sending server is told why, or read it and do neither —
/// which silently drops it, so say which one you meant.
#[derive(Debug, Clone)]
pub struct CfEmailMessage {
    inner: crate::ffi::EmailMessageSys,
}

impl CfEmailMessage {
    /// Create from the raw platform message.
    #[must_use]
    pub const fn new(inner: crate::ffi::EmailMessageSys) -> Self {
        Self { inner }
    }

    /// The envelope sender (SMTP `MAIL FROM`).
    ///
    /// This is the address the sending server authenticated as, not the `From:` header — which is
    /// attacker-controlled and is what phishing forges. Authorization decisions belong here.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn from_address(&self) -> Result<String, CfEventError> {
        self.inner.sender().map_err(js_err)
    }

    /// The envelope recipient (SMTP `RCPT TO`) — the routed address, not the `To:` header.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn to_address(&self) -> Result<String, CfEventError> {
        self.inner.recipient().map_err(js_err)
    }

    /// The parsed message headers.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn headers(&self) -> Result<web_sys::Headers, CfEventError> {
        self.inner.headers().map_err(js_err)
    }

    /// One header by name, case-insensitively, or `None` when it is absent.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn header(&self, name: &str) -> Result<Option<String>, CfEventError> {
        self.headers()?.get(name).map_err(js_err)
    }

    /// The size of the raw RFC 5322 message in bytes.
    ///
    /// Check this before [`raw_bytes`](Self::raw_bytes): a Worker has roughly 128 MB of memory and
    /// an inbound message can be tens of megabytes.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup or reports a non-integer size.
    pub fn raw_size(&self) -> Result<u64, CfEventError> {
        let size = self.inner.raw_size().map_err(js_err)?;
        if !size.is_finite() || size < 0.0 {
            return Err(CfEventError::Runtime(format!(
                "email `rawSize` is not a byte count: {size}"
            )));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(size as u64)
    }

    /// The raw RFC 5322 message as a stream, for parsing without buffering it.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the lookup.
    pub fn raw_stream(&self) -> Result<web_sys::ReadableStream, CfEventError> {
        self.inner.raw().map_err(js_err)
    }

    /// Read the whole raw message into memory.
    ///
    /// Buffers the entire message, so check [`raw_size`](Self::raw_size) first for anything that
    /// might be large. The stream is drained through a `Response`, which is the runtime's own
    /// buffering path rather than a hand-rolled reader loop.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the stream cannot be read.
    pub async fn raw_bytes(&self) -> Result<Vec<u8>, CfEventError> {
        let stream = self.raw_stream()?;
        let response =
            web_sys::Response::new_with_opt_readable_stream(Some(&stream)).map_err(js_err)?;
        let promise = response.array_buffer().map_err(js_err)?;
        let buffer = wasm_bindgen_futures::JsFuture::from(promise)
            .into_send()
            .await
            .map_err(js_err)?;
        Ok(js_sys::Uint8Array::new(&buffer).to_vec())
    }

    /// Reject the message, so the sending server is told why rather than believing it was
    /// delivered.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the call.
    pub fn reject(&self, reason: &str) -> Result<(), CfEventError> {
        self.inner.set_reject(reason).map_err(js_err)
    }

    /// Forward the message to a destination address verified on the account.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the address is not verified or the runtime rejects the forward.
    pub async fn forward(&self, rcpt_to: &str) -> Result<(), CfEventError> {
        self.forward_inner(rcpt_to, &JsValue::UNDEFINED).await
    }

    /// Forward the message, adding headers to the forwarded copy.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the address is not verified or the runtime rejects the forward.
    pub async fn forward_with_headers(
        &self,
        rcpt_to: &str,
        headers: &web_sys::Headers,
    ) -> Result<(), CfEventError> {
        self.forward_inner(rcpt_to, headers.as_ref()).await
    }

    async fn forward_inner(&self, rcpt_to: &str, headers: &JsValue) -> Result<(), CfEventError> {
        let promise = self.inner.forward(rcpt_to, headers).map_err(js_err)?;
        wasm_bindgen_futures::JsFuture::from(promise)
            .into_send()
            .await
            .map_err(js_err)?;
        Ok(())
    }

    /// Reply to the message with an `EmailMessage` value built on the JS side.
    ///
    /// The reply has to be constructed with the `EmailMessage` class from Cloudflare's
    /// `cloudflare:email` module, and `wasm-bindgen` cannot import from a runtime module — so this
    /// takes the constructed value rather than pretending to build one. Pass it in from the worker
    /// shim, or use [`forward`](Self::forward), which needs no such object.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if the runtime rejects the reply.
    pub async fn reply(&self, message: &JsValue) -> Result<(), CfEventError> {
        let promise = self.inner.reply(message).map_err(js_err)?;
        wasm_bindgen_futures::JsFuture::from(promise)
            .into_send()
            .await
            .map_err(js_err)?;
        Ok(())
    }
}

// ── Tail Workers ──

/// One log line a traced Worker emitted.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TailLog {
    /// `log`, `warn`, `error`, `debug`, …
    pub level: Option<String>,
    /// When the line was emitted, in milliseconds since the Unix epoch.
    pub timestamp: Option<i64>,
    /// The arguments passed to the logging call, as an array of arbitrary JSON values.
    pub message: serde_json::Value,
}

/// One uncaught exception from a traced Worker.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TailException {
    /// The error's constructor name.
    pub name: Option<String>,
    /// The error's message.
    pub message: Option<String>,
    /// When it was thrown, in milliseconds since the Unix epoch.
    pub timestamp: Option<i64>,
}

/// One traced invocation of another Worker.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TailTraceItem {
    /// The name of the Worker that produced the trace.
    pub script_name: Option<String>,
    /// How the invocation ended: `ok`, `exception`, `exceededCpu`, `canceled`, …
    pub outcome: Option<String>,
    /// When the invocation started, in milliseconds since the Unix epoch.
    pub event_timestamp: Option<i64>,
    /// Everything the Worker logged.
    pub logs: Vec<TailLog>,
    /// Everything it threw.
    pub exceptions: Vec<TailException>,
    /// CPU milliseconds consumed, when the platform reports it.
    pub cpu_time: Option<f64>,
    /// Wall-clock milliseconds elapsed, when the platform reports it.
    pub wall_time: Option<f64>,
    /// Whether the platform dropped logs or exceptions to stay within its limits.
    ///
    /// A `true` here means the trace is incomplete, which matters a great deal to anything
    /// counting errors downstream.
    pub truncated: bool,
    /// The whole trace item exactly as the platform sent it.
    ///
    /// The `event` field alone is a union over fetch, scheduled, queue, alarm and email
    /// invocations, and the platform keeps extending it — so nothing is discarded.
    #[serde(skip)]
    pub raw: serde_json::Value,
}

/// The batch of traces delivered to an `#[skyzen::tail]` handler.
///
/// A Tail Worker receives the logs and exceptions of *another* Worker, which is how an
/// observability pipeline is built on Workers: forward them to a queue, a log sink or an analytics
/// dataset.
#[derive(Debug, Clone)]
pub struct CfTailEvent {
    inner: js_sys::Array,
}

impl CfTailEvent {
    /// Create from the raw array of trace items.
    #[must_use]
    pub const fn new(inner: js_sys::Array) -> Self {
        Self { inner }
    }

    /// The raw array, for fields the typed shape does not name.
    #[must_use]
    pub const fn raw(&self) -> &js_sys::Array {
        &self.inner
    }

    /// How many invocations this batch covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.length() as usize
    }

    /// Whether the batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Decode the batch into typed trace items.
    ///
    /// Decoding goes through `serde_json::Value` so the untouched item survives into
    /// [`TailTraceItem::raw`], and so the typed pass is plain serde.
    ///
    /// # Errors
    ///
    /// Returns [`CfEventError`] if a trace item is not a plain JSON object or has an unexpected
    /// field type.
    pub fn traces(&self) -> Result<Vec<TailTraceItem>, CfEventError> {
        self.inner
            .iter()
            .map(|item| {
                let raw: serde_json::Value = serde_wasm_bindgen::from_value(item)?;
                let mut trace: TailTraceItem = serde_json::from_value(raw.clone())
                    .map_err(|error| CfEventError::Decode(error.to_string()))?;
                trace.raw = raw;
                Ok(trace)
            })
            .collect()
    }
}

#[allow(clippy::needless_pass_by_value)]
fn js_err(error: JsValue) -> CfEventError {
    CfEventError::Runtime(format!("{error:?}"))
}

fn queue_retry_to_cf(retry: QueueRetry) -> CfQueueRetryOptions {
    retry
        .delay_seconds
        .map_or_else(CfQueueRetryOptions::new, |delay_seconds| {
            CfQueueRetryOptions::new().with_delay_seconds(delay_seconds)
        })
}
