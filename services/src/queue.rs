//! Message queue abstraction.
//!
//! Provides a platform-agnostic interface for message queues.
//! Implementations include SQS, Cloudflare Queues, Azure Service Bus,
//! and in-memory (for testing).

use core::future::Future;

use http_kit::http_error;
use serde::Serialize;
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
