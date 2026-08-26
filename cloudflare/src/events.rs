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

/// A Cloudflare queue consumer context.
#[derive(Debug)]
pub struct CfQueueContext {
    inner: worker_sys::Context,
}

impl CfQueueContext {
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
