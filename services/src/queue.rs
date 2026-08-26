//! Message queue abstraction.
//!
//! Provides a platform-agnostic interface for message queues.
//! Implementations include SQS, Cloudflare Queues, Azure Service Bus,
//! and in-memory (for testing).

use core::future::Future;

use serde::{de::DeserializeOwned, Serialize};

// ── Error type ──

/// Errors that can occur during message queue operations.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// The underlying queue backend returned an error.
    #[error("queue error: {message}")]
    Backend {
        /// A human-readable description of what the backend was asked to do.
        message: String,
        /// The backend's own error, when it hands one back.
        #[source]
        source: Option<crate::BoxError>,
    },

    /// Serialization failed.
    #[error("queue serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The backend does not support the requested operation.
    #[error("unsupported queue operation: {0}")]
    Unsupported(&'static str),

    /// A conditional operation failed because the queue state changed underneath it.
    #[error("queue conflict: the message state changed before the operation was applied")]
    Conflict,

    /// The backend rejected the request because the caller is over its rate limit.
    #[error("queue request was throttled by the backend")]
    Throttled {
        /// How long the backend asked the caller to wait, when it says.
        retry_after: Option<core::time::Duration>,
    },

    /// The configured credentials were rejected by the backend.
    #[error("queue credentials were rejected by the backend")]
    Unauthorized,

    /// A [`send_batch`](MessageQueue::send_batch) enqueued some of its messages and not others.
    ///
    /// Batch sends are not atomic on any backend that has them, so this reports *which* messages
    /// were rejected rather than leaving the caller to guess: every message the batch carried whose
    /// index is absent from `failures` was enqueued, and re-sending only the failures is what
    /// recovers the batch.
    #[error("{} of the batch's messages were rejected by the backend", failures.len())]
    PartialBatch {
        /// The rejected entries, each naming its position in the slice the caller passed.
        failures: Vec<BatchSendFailure>,
    },
}

/// One message a [`send_batch`](MessageQueue::send_batch) could not enqueue.
///
/// `index` is the position in the slice the caller handed to `send_batch`, not the backend's own
/// per-request entry id, so it indexes the caller's data directly however the backend chunked the
/// request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSendFailure {
    /// Position of the rejected message in the slice passed to `send_batch`.
    pub index: usize,
    /// The backend's error code for this entry, verbatim.
    pub code: String,
    /// The backend's human-readable message for this entry.
    pub message: String,
}

backend_error!(QueueError);

service_http_error!(QueueError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Serialization(_) => INTERNAL_SERVER_ERROR,
    Self::Unsupported(_) => NOT_IMPLEMENTED,
    Self::Conflict => CONFLICT,
    Self::Throttled { .. } => TOO_MANY_REQUESTS,
    Self::Unauthorized => INTERNAL_SERVER_ERROR,
    Self::PartialBatch { .. } => INTERNAL_SERVER_ERROR,
});

/// Retry options for queue consumers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueRetry {
    /// Delay the retry by the given number of seconds, if supported by the provider.
    pub delay_seconds: Option<u32>,
}

impl QueueRetry {
    /// Create empty retry options.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            delay_seconds: None,
        }
    }

    /// Delay the retry by the given number of seconds.
    #[must_use]
    pub const fn with_delay_seconds(mut self, delay_seconds: u32) -> Self {
        self.delay_seconds = Some(delay_seconds);
        self
    }
}

/// A single queue message delivered to consumer code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueMessage<T> {
    /// The provider-assigned message identifier.
    pub id: String,
    /// The message timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: i64,
    /// The decoded message body.
    pub body: T,
}

impl<T> QueueMessage<T> {
    /// Map the message body while preserving message metadata.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> QueueMessage<U> {
        QueueMessage {
            id: self.id,
            timestamp_ms: self.timestamp_ms,
            body: f(self.body),
        }
    }
}

/// A decoded queue batch delivered to consumer code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueBatch<T> {
    /// The queue name that produced this batch.
    pub queue: String,
    /// The messages in the delivered batch.
    pub messages: Vec<QueueMessage<T>>,
}

impl<T> QueueBatch<T> {
    /// Return the number of messages in the batch.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.messages.len()
    }

    /// Return whether the batch is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Map the message bodies while preserving batch metadata.
    pub fn map<U>(self, mut f: impl FnMut(T) -> U) -> QueueBatch<U> {
        QueueBatch {
            queue: self.queue,
            messages: self
                .messages
                .into_iter()
                .map(|message| message.map(&mut f))
                .collect(),
        }
    }

    /// Encode the entire batch body as JSON bytes.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError`] if any message body fails to serialize.
    pub fn encode_json(self) -> Result<Vec<Vec<u8>>, QueueError>
    where
        T: Serialize,
    {
        self.messages
            .into_iter()
            .map(|message| serde_json::to_vec(&message.body).map_err(Into::into))
            .collect()
    }
}

impl QueueBatch<Vec<u8>> {
    /// Decode each message body from JSON.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError`] if any message body fails to deserialize.
    pub fn decode_json<T: DeserializeOwned>(self) -> Result<QueueBatch<T>, QueueError> {
        let messages = self
            .messages
            .into_iter()
            .map(|message| {
                serde_json::from_slice::<T>(&message.body)
                    .map(|body| QueueMessage {
                        id: message.id,
                        timestamp_ms: message.timestamp_ms,
                        body,
                    })
                    .map_err(Into::into)
            })
            .collect::<Result<Vec<_>, QueueError>>()?;

        Ok(QueueBatch {
            queue: self.queue,
            messages,
        })
    }
}

/// Options for producing a message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SendOptions {
    /// Hold the message invisible for this long before any consumer may receive it.
    ///
    /// This is the normal way to schedule deferred work: SQS calls it `DelaySeconds` and
    /// Cloudflare Queues `delaySeconds`. It is unrelated to [`QueueRetry::delay_seconds`], which
    /// delays the *redelivery* of a message that was already consumed.
    pub delay: Option<core::time::Duration>,
}

impl SendOptions {
    /// Options that deliver the message as soon as the backend can.
    #[must_use]
    pub const fn new() -> Self {
        Self { delay: None }
    }

    /// Hold the message for `delay` before it becomes visible to consumers.
    #[must_use]
    pub const fn with_delay(mut self, delay: core::time::Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

/// Options for one pull-based [`MessageQueue::receive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceiveOptions {
    /// How many messages to take at most. Backends cap this at their own batch size.
    pub max_messages: usize,
    /// How long the received messages stay invisible to other consumers before the backend
    /// redelivers them. `None` uses the queue's configured default.
    pub visibility_timeout: Option<core::time::Duration>,
    /// How long the backend may wait for a message before returning empty (long polling).
    /// `None` returns whatever is available immediately.
    pub wait: Option<core::time::Duration>,
}

impl Default for ReceiveOptions {
    /// One message, the queue's default visibility timeout, no long poll.
    fn default() -> Self {
        Self {
            max_messages: 1,
            visibility_timeout: None,
            wait: None,
        }
    }
}

impl ReceiveOptions {
    /// One message, the queue's default visibility timeout, no long poll.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_messages: 1,
            visibility_timeout: None,
            wait: None,
        }
    }

    /// Take up to `max_messages` in one call.
    #[must_use]
    pub const fn with_max_messages(mut self, max_messages: usize) -> Self {
        self.max_messages = max_messages;
        self
    }

    /// Keep the received messages invisible for `visibility_timeout`.
    #[must_use]
    pub const fn with_visibility_timeout(
        mut self,
        visibility_timeout: core::time::Duration,
    ) -> Self {
        self.visibility_timeout = Some(visibility_timeout);
        self
    }

    /// Let the backend long-poll for up to `wait` before returning empty.
    #[must_use]
    pub const fn with_wait(mut self, wait: core::time::Duration) -> Self {
        self.wait = Some(wait);
        self
    }
}

/// The lease a consumer holds on a received message.
///
/// The contents are the provider's own settle token — an SQS receipt handle, a Service Bus lock
/// token, an Azure Storage queue pop receipt — and are opaque to Skyzen: pass the value back to
/// [`MessageQueue::ack`] or [`MessageQueue::nack`] rather than interpreting it. A receipt belongs
/// to one delivery, so it stops being valid once the message is settled or its visibility timeout
/// lapses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageReceipt(String);

impl MessageReceipt {
    /// Wrap a provider's settle token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The provider's settle token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A message handed to a pull-based consumer, together with the lease that settles it.
///
/// The body is raw bytes as delivered; [`Queue::receive_json`] returns `ReceivedMessage<T>` with
/// the body already decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedMessage<T = Vec<u8>> {
    /// The provider-assigned message identifier, when the provider assigns one.
    pub id: Option<String>,
    /// The message body.
    pub body: T,
    /// The lease to hand back to [`MessageQueue::ack`] or [`MessageQueue::nack`].
    pub receipt: MessageReceipt,
    /// How many times this message has been delivered, counting this one, when the provider
    /// tracks it. A value above 1 means an earlier delivery was not acknowledged.
    pub attempts: Option<u32>,
}

impl<T> ReceivedMessage<T> {
    /// Map the body while keeping the identity and the lease.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> ReceivedMessage<U> {
        ReceivedMessage {
            id: self.id,
            body: f(self.body),
            receipt: self.receipt,
            attempts: self.attempts,
        }
    }
}

impl ReceivedMessage {
    /// Decode the body from JSON, keeping the identity and the lease.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Serialization`] if the body is not valid JSON for `T`.
    pub fn decode_json<T: DeserializeOwned>(self) -> Result<ReceivedMessage<T>, QueueError> {
        let body = serde_json::from_slice(&self.body)?;
        Ok(ReceivedMessage {
            id: self.id,
            body,
            receipt: self.receipt,
            attempts: self.attempts,
        })
    }
}

/// The acknowledgement or retry decision for a single queue message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMessageDisposition {
    /// Acknowledge the message as successfully processed.
    Ack,
    /// Retry the message.
    Retry(QueueRetry),
}

/// The acknowledgement or retry decision for a queue batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueBatchDisposition {
    /// Apply the same decision to every message in the batch.
    All(QueueMessageDisposition),
    /// Apply one decision per message in the batch, preserving order.
    PerMessage(Vec<QueueMessageDisposition>),
}

impl QueueBatchDisposition {
    /// Acknowledge every message in the batch.
    #[must_use]
    pub const fn ack_all() -> Self {
        Self::All(QueueMessageDisposition::Ack)
    }

    /// Retry every message in the batch.
    #[must_use]
    pub const fn retry_all(retry: QueueRetry) -> Self {
        Self::All(QueueMessageDisposition::Retry(retry))
    }
}

// ── Layer 1: Public trait ──

/// A platform-agnostic message queue interface.
///
/// Messages are raw bytes. Serialization is the caller's responsibility,
/// or use the convenience methods on [`Queue`] for JSON serialization.
///
/// Implementors provide concrete queue backends (SQS, CF Queues, etc.).
/// User code interacts through the [`Queue`] wrapper, never this trait directly.
///
/// # Producing and consuming
///
/// Every backend produces. Consumption comes in two shapes and the trait covers both:
///
/// - **Push**, where the platform invokes your worker with a batch. Cloudflare Queues works this
///   way, and Skyzen surfaces it through `#[skyzen::queue]` with [`QueueBatch`] and
///   [`QueueBatchDisposition`]; [`receive`](MessageQueue::receive) is not involved.
/// - **Pull**, where the consumer asks for messages and settles them itself. SQS and Azure Service
///   Bus work this way; that is what [`receive`](MessageQueue::receive),
///   [`ack`](MessageQueue::ack) and [`nack`](MessageQueue::nack) are for.
///
/// A push-only backend leaves the pull methods at their [`QueueError::Unsupported`] defaults, so
/// asking a Cloudflare queue to `receive` fails loudly rather than returning a silent empty batch.
pub trait MessageQueue: Send + Sync + Clone + 'static {
    /// Send a single message.
    fn send(&self, message: &[u8]) -> impl Future<Output = Result<(), QueueError>> + Send;

    /// Send a batch of messages.
    ///
    /// Delivery is at-least-once and **not atomic** on any backend: a backend that rejects some
    /// entries while accepting others reports [`QueueError::PartialBatch`], whose
    /// [`BatchSendFailure::index`] values index this very slice, so the caller can retry exactly
    /// the messages that did not make it.
    fn send_batch(
        &self,
        messages: &[Vec<u8>],
    ) -> impl Future<Output = Result<(), QueueError>> + Send;

    /// Send a single message with per-message delivery options.
    ///
    /// The default forwards to [`send`](MessageQueue::send) when `options` asks for nothing and
    /// returns [`QueueError::Unsupported`] otherwise, so a backend without delayed delivery cannot
    /// silently turn a scheduled message into an immediate one.
    fn send_with(
        &self,
        message: &[u8],
        options: SendOptions,
    ) -> impl Future<Output = Result<(), QueueError>> + Send {
        async move {
            if options.delay.is_none() {
                self.send(message).await
            } else {
                Err(QueueError::Unsupported(
                    "delayed delivery is not supported by this queue backend",
                ))
            }
        }
    }

    /// Take up to [`ReceiveOptions::max_messages`] messages, leasing each one.
    ///
    /// Each returned message stays invisible to other consumers until it is settled with
    /// [`ack`](MessageQueue::ack) / [`nack`](MessageQueue::nack) or its visibility timeout lapses,
    /// at which point the backend redelivers it. Push-only backends return
    /// [`QueueError::Unsupported`].
    fn receive(
        &self,
        options: ReceiveOptions,
    ) -> impl Future<Output = Result<Vec<ReceivedMessage>, QueueError>> + Send {
        let _ = options;
        async {
            Err(QueueError::Unsupported(
                "pull-based consumption is not supported by this queue backend; \
                 messages are delivered by the platform to a #[skyzen::queue] handler",
            ))
        }
    }

    /// Acknowledge a received message so the backend deletes it.
    fn ack(&self, receipt: &MessageReceipt) -> impl Future<Output = Result<(), QueueError>> + Send {
        let _ = receipt;
        async {
            Err(QueueError::Unsupported(
                "settling a message is not supported by this queue backend",
            ))
        }
    }

    /// Return a received message to the queue for redelivery.
    fn nack(
        &self,
        receipt: &MessageReceipt,
        retry: QueueRetry,
    ) -> impl Future<Output = Result<(), QueueError>> + Send {
        let _ = (receipt, retry);
        async {
            Err(QueueError::Unsupported(
                "settling a message is not supported by this queue backend",
            ))
        }
    }
}

// ── Layer 2: Generated object-safe trait ──

service_obj! {
    MessageQueueObj: MessageQueue;
    async fn send<'a>(&'a self, message: &'a [u8]) -> Result<(), QueueError>;
    async fn send_batch<'a>(&'a self, messages: &'a [Vec<u8>]) -> Result<(), QueueError>;
    async fn send_with<'a>(
        &'a self,
        message: &'a [u8],
        options: SendOptions,
    ) -> Result<(), QueueError>;
    async fn receive(&'_ self, options: ReceiveOptions) -> Result<Vec<ReceivedMessage>, QueueError>;
    async fn ack<'a>(&'a self, receipt: &'a MessageReceipt) -> Result<(), QueueError>;
    async fn nack<'a>(
        &'a self,
        receipt: &'a MessageReceipt,
        retry: QueueRetry,
    ) -> Result<(), QueueError>;
}

// ── User-facing wrapper ──

/// A type-erased message queue extractor.
///
/// `Queue` wraps any [`MessageQueue`] implementation behind dynamic dispatch.
/// It is injected into handlers via request extensions.
pub struct Queue(Box<dyn MessageQueueObj>);

service_extractor!(
    Queue,
    QueueNotConfigured,
    "Message queue not configured. Ensure a MessageQueue implementation is injected."
);

impl Queue {
    /// Create a new `Queue` from any [`MessageQueue`] implementation.
    pub fn new(queue: impl MessageQueue) -> Self {
        Self(Box::new(queue))
    }

    /// Send a single raw message.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError`] if the backend operation fails.
    pub async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        self.0.send(message).await
    }

    /// Send a batch of raw messages.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::PartialBatch`] when the backend rejected some entries and enqueued
    /// the rest — its [`BatchSendFailure::index`] values index `messages` — or another
    /// [`QueueError`] if the backend operation fails outright.
    pub async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        self.0.send_batch(messages).await
    }

    /// Send a single raw message with per-message delivery options.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Unsupported`] if the backend cannot honour the requested options, or
    /// another [`QueueError`] if the backend operation fails.
    pub async fn send_with(&self, message: &[u8], options: SendOptions) -> Result<(), QueueError> {
        self.0.send_with(message, options).await
    }

    /// Take up to [`ReceiveOptions::max_messages`] leased messages from the queue.
    ///
    /// Settle each one with [`ack`](Self::ack) or [`nack`](Self::nack); anything left unsettled is
    /// redelivered once its visibility timeout lapses.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Unsupported`] on a push-delivery backend, or another [`QueueError`]
    /// if the backend operation fails.
    pub async fn receive(
        &self,
        options: ReceiveOptions,
    ) -> Result<Vec<ReceivedMessage>, QueueError> {
        self.0.receive(options).await
    }

    /// Take up to [`ReceiveOptions::max_messages`] leased messages and decode each body from JSON.
    ///
    /// A message whose body is not valid JSON for `T` fails the whole call and stays leased, so it
    /// returns to the queue when its visibility timeout lapses rather than being lost.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Serialization`] if a body cannot be decoded, or any error
    /// [`receive`](Self::receive) reports.
    pub async fn receive_json<T: DeserializeOwned>(
        &self,
        options: ReceiveOptions,
    ) -> Result<Vec<ReceivedMessage<T>>, QueueError> {
        self.receive(options)
            .await?
            .into_iter()
            .map(ReceivedMessage::decode_json)
            .collect()
    }

    /// Acknowledge a received message so the backend deletes it.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Unsupported`] on a push-delivery backend, or another [`QueueError`]
    /// if the backend operation fails.
    pub async fn ack(&self, receipt: &MessageReceipt) -> Result<(), QueueError> {
        self.0.ack(receipt).await
    }

    /// Return a received message to the queue for redelivery.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Unsupported`] on a push-delivery backend, or another [`QueueError`]
    /// if the backend operation fails.
    pub async fn nack(
        &self,
        receipt: &MessageReceipt,
        retry: QueueRetry,
    ) -> Result<(), QueueError> {
        self.0.nack(receipt, retry).await
    }

    /// Serialize a value to JSON and send it as a message.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError`] if serialization or the backend operation fails.
    pub async fn send_json<T: Serialize + Sync>(&self, message: &T) -> Result<(), QueueError> {
        let bytes = serde_json::to_vec(message)?;
        self.send(&bytes).await
    }

    /// Serialize multiple values to JSON and send them as a batch.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError`] if serialization or the backend operation fails.
    pub async fn send_json_batch<T: Serialize + Sync>(
        &self,
        messages: &[T],
    ) -> Result<(), QueueError> {
        let batch: Result<Vec<Vec<u8>>, _> = messages.iter().map(serde_json::to_vec).collect();
        self.send_batch(&batch?).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MessageQueue, MessageReceipt, Queue, QueueBatch, QueueError, QueueMessage, QueueRetry,
        ReceiveOptions, ReceivedMessage, SendOptions,
    };
    use core::future::{ready, Future};
    use http_kit::{Body, Endpoint, HttpError, Response};
    use serde::{Deserialize, Serialize};
    use skyzen_core::Extractor;
    use std::{
        convert::Infallible,
        sync::{Arc, RwLock, RwLockWriteGuard},
    };

    #[derive(Clone, Default)]
    struct InMemoryMessageQueue {
        messages: Arc<RwLock<Vec<Vec<u8>>>>,
    }

    impl InMemoryMessageQueue {
        fn write(&self) -> Result<RwLockWriteGuard<'_, Vec<Vec<u8>>>, QueueError> {
            self.messages
                .write()
                .map_err(|_| QueueError::backend("lock poisoned"))
        }
    }

    // Pushing onto a `Vec` behind a lock is synchronous, so the futures are ready on creation
    // rather than `async` blocks with nothing to await.
    impl MessageQueue for InMemoryMessageQueue {
        fn send(&self, message: &[u8]) -> impl Future<Output = Result<(), QueueError>> + Send {
            ready(self.write().map(|mut messages| {
                messages.push(message.to_vec());
            }))
        }

        fn send_batch(
            &self,
            messages: &[Vec<u8>],
        ) -> impl Future<Output = Result<(), QueueError>> + Send {
            ready(self.write().map(|mut queued| {
                queued.extend(messages.iter().cloned());
            }))
        }
    }

    #[derive(Debug, Clone)]
    struct QueueSendEndpoint;

    impl Endpoint for QueueSendEndpoint {
        type Error = Infallible;

        async fn respond(
            &mut self,
            request: &mut http_kit::Request,
        ) -> Result<Response, Self::Error> {
            let queue = Queue::extract(request)
                .await
                .expect("queue should be injected");
            queue
                .send(b"from-endpoint")
                .await
                .expect("queue send should succeed");
            Ok(Response::new(Body::from("queued")))
        }
    }

    #[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
    struct Job {
        kind: String,
    }

    #[test]
    fn decodes_queue_batch_json() {
        let batch = QueueBatch {
            queue: "jobs".to_owned(),
            messages: vec![QueueMessage {
                id: "1".to_owned(),
                timestamp_ms: 10,
                body: br#"{"kind":"email"}"#.to_vec(),
            }],
        };

        let decoded = batch.decode_json::<Job>().expect("batch should decode");
        assert_eq!(decoded.queue, "jobs");
        assert_eq!(decoded.messages[0].body.kind, "email");
    }

    #[test]
    fn queue_batch_helpers_preserve_metadata_and_encode_json() {
        let batch = QueueBatch {
            queue: "jobs".to_owned(),
            messages: vec![
                QueueMessage {
                    id: "1".to_owned(),
                    timestamp_ms: 10,
                    body: Job {
                        kind: "email".to_owned(),
                    },
                },
                QueueMessage {
                    id: "2".to_owned(),
                    timestamp_ms: 20,
                    body: Job {
                        kind: "sms".to_owned(),
                    },
                },
            ],
        };

        assert_eq!(batch.len(), 2);
        assert!(!batch.is_empty());

        let mapped = batch.clone().map(|job| job.kind);
        assert_eq!(mapped.messages[0].id, "1");
        assert_eq!(mapped.messages[1].body, "sms");

        let encoded = batch.encode_json().expect("batch should encode");
        assert_eq!(encoded.len(), 2);
        assert!(std::str::from_utf8(&encoded[0]).unwrap().contains("email"));
    }

    #[test]
    fn decode_json_surfaces_invalid_message_payloads() {
        let batch = QueueBatch {
            queue: "jobs".to_owned(),
            messages: vec![QueueMessage {
                id: "1".to_owned(),
                timestamp_ms: 10,
                body: b"invalid-json".to_vec(),
            }],
        };

        let error = batch.decode_json::<Job>().unwrap_err();
        assert!(matches!(error, QueueError::Serialization(_)));
    }

    #[tokio::test]
    async fn wrapper_sends_raw_and_json_messages() {
        let backend = InMemoryMessageQueue::default();
        let queue = Queue::new(backend.clone());

        queue.send(b"raw").await.unwrap();
        queue
            .send_json(&Job {
                kind: "email".to_owned(),
            })
            .await
            .unwrap();
        queue
            .send_json_batch(&[
                Job {
                    kind: "sms".to_owned(),
                },
                Job {
                    kind: "push".to_owned(),
                },
            ])
            .await
            .unwrap();

        let messages = backend.messages.read().unwrap().clone();
        assert_eq!(messages[0], b"raw".to_vec());
        assert!(std::str::from_utf8(&messages[1]).unwrap().contains("email"));
        assert!(std::str::from_utf8(&messages[2]).unwrap().contains("sms"));
        assert!(std::str::from_utf8(&messages[3]).unwrap().contains("push"));
    }

    #[tokio::test]
    async fn middleware_injects_queue_for_downstream_endpoint_and_extractor() {
        let backend = InMemoryMessageQueue::default();
        let queue = Queue::new(backend.clone());
        let mut request = http_kit::Request::new(Body::empty());

        let response = ::skyzen_core::middleware::apply(&queue, &mut request, QueueSendEndpoint)
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "queued");

        let extracted = Queue::extract(&mut request).await.unwrap();
        extracted.send(b"from-extractor").await.unwrap();

        let messages = backend.messages.read().unwrap().clone();
        assert_eq!(
            messages,
            vec![b"from-endpoint".to_vec(), b"from-extractor".to_vec()]
        );
    }

    #[tokio::test]
    async fn send_with_forwards_a_plain_send_and_refuses_an_unsupported_delay() {
        let backend = InMemoryMessageQueue::default();
        let queue = Queue::new(backend.clone());

        queue.send_with(b"now", SendOptions::new()).await.unwrap();
        assert_eq!(
            backend.messages.read().unwrap().clone(),
            vec![b"now".to_vec()]
        );

        let error = queue
            .send_with(
                b"later",
                SendOptions::new().with_delay(core::time::Duration::from_mins(5)),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, QueueError::Unsupported(_)));

        // The refused send must not have enqueued anything.
        assert_eq!(backend.messages.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pull_consumption_defaults_to_unsupported_on_a_push_only_backend() {
        let queue = Queue::new(InMemoryMessageQueue::default());
        let receipt = MessageReceipt::new("lease-1");

        assert!(matches!(
            queue.receive(ReceiveOptions::new()).await.unwrap_err(),
            QueueError::Unsupported(_)
        ));
        assert!(matches!(
            queue.ack(&receipt).await.unwrap_err(),
            QueueError::Unsupported(_)
        ));
        assert!(matches!(
            queue.nack(&receipt, QueueRetry::new()).await.unwrap_err(),
            QueueError::Unsupported(_)
        ));
    }

    #[test]
    fn received_message_decodes_its_body_while_keeping_the_lease() {
        let message = ReceivedMessage {
            id: Some("1".to_owned()),
            body: br#"{"kind":"email"}"#.to_vec(),
            receipt: MessageReceipt::new("lease-1"),
            attempts: Some(2),
        };

        let decoded = message.decode_json::<Job>().expect("body should decode");
        assert_eq!(decoded.body.kind, "email");
        assert_eq!(decoded.receipt.as_str(), "lease-1");
        assert_eq!(decoded.attempts, Some(2));

        let malformed = ReceivedMessage {
            id: None,
            body: b"not-json".to_vec(),
            receipt: MessageReceipt::new("lease-2"),
            attempts: None,
        };
        assert!(matches!(
            malformed.decode_json::<Job>().unwrap_err(),
            QueueError::Serialization(_)
        ));
    }

    #[test]
    fn receive_options_default_to_a_single_message() {
        assert_eq!(ReceiveOptions::default(), ReceiveOptions::new());
        assert_eq!(ReceiveOptions::default().max_messages, 1);
        assert!(ReceiveOptions::default().visibility_timeout.is_none());
        assert!(ReceiveOptions::default().wait.is_none());
    }

    #[tokio::test]
    async fn extractor_returns_internal_server_error_when_queue_is_missing() {
        let mut request = http_kit::Request::new(Body::empty());

        let error = Queue::extract(&mut request).await.unwrap_err();

        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
