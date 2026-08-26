//! Message queue abstraction.
//!
//! Provides a platform-agnostic interface for message queues.
//! Implementations include SQS, Cloudflare Queues, Azure Service Bus,
//! and in-memory (for testing).

use core::future::Future;

use serde::{de::DeserializeOwned, Serialize};

use crate::maybe_send::{BoxFuture, MaybeSend};

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
}

backend_error!(QueueError);

service_http_error!(QueueError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Serialization(_) => INTERNAL_SERVER_ERROR,
    Self::Conflict => CONFLICT,
    Self::Throttled { .. } => TOO_MANY_REQUESTS,
    Self::Unauthorized => INTERNAL_SERVER_ERROR,
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

#[cfg(test)]
mod tests {
    use super::{MessageQueue, Queue, QueueBatch, QueueError, QueueMessage};
    use http_kit::{Body, Endpoint, HttpError, Middleware, Response};
    use serde::{Deserialize, Serialize};
    use skyzen_core::Extractor;
    use std::{
        convert::Infallible,
        sync::{Arc, RwLock},
    };

    #[derive(Clone, Default)]
    struct InMemoryMessageQueue {
        messages: Arc<RwLock<Vec<Vec<u8>>>>,
    }

    impl MessageQueue for InMemoryMessageQueue {
        async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
            self.messages
                .write()
                .map_err(|_| QueueError::backend("lock poisoned"))?
                .push(message.to_vec());
            Ok(())
        }

        async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
            self.messages
                .write()
                .map_err(|_| QueueError::backend("lock poisoned"))?
                .extend(messages.iter().cloned());
            Ok(())
        }
    }

    #[derive(Debug)]
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
        let mut queue = Queue::new(backend.clone());
        let mut request = http_kit::Request::new(Body::empty());

        let response = queue.handle(&mut request, QueueSendEndpoint).await.unwrap();
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
    async fn extractor_returns_internal_server_error_when_queue_is_missing() {
        let mut request = http_kit::Request::new(Body::empty());

        let error = Queue::extract(&mut request).await.unwrap_err();

        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
