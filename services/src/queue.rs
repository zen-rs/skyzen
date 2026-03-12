//! Message queue abstraction.
//!
//! Provides a platform-agnostic interface for message queues.
//! Implementations include SQS, Cloudflare Queues, Azure Service Bus,
//! and in-memory (for testing).

use core::{convert::Infallible, future::Future};

use http_kit::{
    http_error,
    middleware::{Middleware, MiddlewareError},
    Endpoint, Response,
};
use serde::{de::DeserializeOwned, Serialize};
use skyzen_core::{Extractor, StatusCode};

use crate::maybe_send::{BoxFuture, MaybeSend};

// ── Error type ──

/// Errors that can occur during message queue operations.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// The underlying queue backend returned an error.
    #[error("queue error: {0}")]
    Backend(String),

    /// Serialization failed.
    #[error("queue serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

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
pub trait MessageQueue: Send + Sync + Clone + 'static {
    /// Send a single message.
    fn send(&self, message: &[u8]) -> impl Future<Output = Result<(), QueueError>> + MaybeSend;

    /// Send a batch of messages.
    fn send_batch(
        &self,
        messages: &[Vec<u8>],
    ) -> impl Future<Output = Result<(), QueueError>> + MaybeSend;
}

// ── Layer 2: Private object-safe trait ──

trait MessageQueueObj: Send + Sync {
    fn send<'a>(&'a self, message: &'a [u8]) -> BoxFuture<'a, Result<(), QueueError>>;
    fn send_batch<'a>(&'a self, messages: &'a [Vec<u8>]) -> BoxFuture<'a, Result<(), QueueError>>;
    fn clone_box(&self) -> Box<dyn MessageQueueObj>;
}

// ── Bridge ──

impl<T: MessageQueue> MessageQueueObj for T {
    fn send<'a>(&'a self, message: &'a [u8]) -> BoxFuture<'a, Result<(), QueueError>> {
        Box::pin(MessageQueue::send(self, message))
    }

    fn send_batch<'a>(&'a self, messages: &'a [Vec<u8>]) -> BoxFuture<'a, Result<(), QueueError>> {
        Box::pin(MessageQueue::send_batch(self, messages))
    }

    fn clone_box(&self) -> Box<dyn MessageQueueObj> {
        Box::new(self.clone())
    }
}

// ── User-facing wrapper ──

/// A type-erased message queue extractor.
///
/// `Queue` wraps any [`MessageQueue`] implementation behind dynamic dispatch.
/// It is injected into handlers via request extensions.
pub struct Queue(Box<dyn MessageQueueObj>);

impl Clone for Queue {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for Queue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue").finish_non_exhaustive()
    }
}

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
    /// Returns [`QueueError`] if the backend operation fails.
    pub async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        self.0.send_batch(messages).await
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

http_error!(
    /// The queue service was not found in request extensions.
    pub QueueNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Message queue not configured. Ensure a MessageQueue implementation is injected."
);

impl Extractor for Queue {
    type Error = QueueNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(QueueNotConfigured::new())
    }
}

impl Middleware for Queue {
    type Error = Infallible;

    async fn handle<N: Endpoint>(
        &mut self,
        request: &mut http_kit::Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        request.extensions_mut().insert(self.clone());
        next.respond(request)
            .await
            .map_err(MiddlewareError::Endpoint)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QueueBatch, QueueBatchDisposition, QueueMessage, QueueMessageDisposition, QueueRetry,
    };

    #[test]
    fn decodes_queue_batch_json() {
        #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
        struct Job {
            kind: String,
        }

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
    fn queue_retry_builder_sets_delay() {
        let retry = QueueRetry::new().with_delay_seconds(30);
        assert_eq!(retry.delay_seconds, Some(30));
    }

    #[test]
    fn queue_batch_ack_all_helper_is_stable() {
        assert_eq!(
            QueueBatchDisposition::ack_all(),
            QueueBatchDisposition::All(QueueMessageDisposition::Ack)
        );
    }
}
