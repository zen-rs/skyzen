//! What a Lambda does with an SQS batch it was handed.

use core::future::Future;

use skyzen_services::{
    queue::{QueueBatch, QueueBatchDisposition},
    BoxError,
};

/// Drives the application's `#[skyzen::queue]` handler for a batch the platform pushed.
///
/// Lambda invokes a function; it does not let it poll. So where the native runtime runs its own
/// receive loop, here the batch arrives already leased and this trait is the only thing standing
/// between the event and the handler.
///
/// The `skyzen` crate implements this for the consumer set `#[skyzen::main]` builds, so an
/// application never names it. Implement it directly only when embedding Skyzen by hand.
pub trait QueueDispatch: Send + Sync + 'static {
    /// Whether the application declares a queue handler at all.
    ///
    /// A const rather than a method: a Lambda wired to an SQS event source but built from an
    /// application with no `#[skyzen::queue]` handler can never process a message, and saying so
    /// costs nothing at runtime.
    const DECLARED: bool;

    /// Handle one pushed batch and say how its messages should be settled.
    ///
    /// # Errors
    ///
    /// Returns the handler's own error. Every message in the batch is then reported as failed, so
    /// SQS redelivers the whole batch.
    fn dispatch(
        &self,
        batch: QueueBatch<Vec<u8>>,
    ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send;
}

/// The dispatcher of an application that declares no queue handler.
///
/// Serving HTTP is the whole job of most Lambdas, and this is what they pass. An SQS event that
/// reaches one is refused by name rather than acknowledged and dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoQueueHandler;

impl QueueDispatch for NoQueueHandler {
    const DECLARED: bool = false;

    fn dispatch(
        &self,
        _batch: QueueBatch<Vec<u8>>,
    ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send {
        // Unreachable in practice: `run` refuses an SQS event before it gets this far when
        // `DECLARED` is false. Spelled out anyway, because a hand-written dispatcher could set
        // the const and forget the method.
        core::future::ready(Err(BoxError::from(
            "this application declares no #[skyzen::queue] handler",
        )))
    }
}
