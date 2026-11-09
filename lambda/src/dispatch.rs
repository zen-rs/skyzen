//! What a Lambda does with the platform-pushed events it was handed.

use core::future::Future;

use skyzen_services::{
    queue::{QueueBatch, QueueBatchDisposition},
    BoxError, ScheduledTick,
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

/// Drives the application's `#[skyzen::scheduled]` handler for a tick the platform pushed.
///
/// `EventBridge` owns the timing on Lambda exactly as the native scheduler loop does at home
/// and Cloudflare's Cron Triggers do on the edge; what reaches the function is the same
/// [`ScheduledTick`] either way.
pub trait ScheduledDispatch: Send + Sync + 'static {
    /// Whether the application declares a scheduled handler at all.
    ///
    /// A const rather than a method, so a scheduled event that reaches a function with nothing
    /// to give it to can be refused by name.
    const DECLARED: bool;

    /// Handle one pushed tick.
    ///
    /// # Errors
    ///
    /// Returns the handler's own error, which fails the invocation. `EventBridge` retries failed
    /// invocations the same way it retries any failed scheduled event — the handler's `Err` is
    /// what "the tick failed" means here.
    fn dispatch(&self, tick: ScheduledTick) -> impl Future<Output = Result<(), BoxError>> + Send;
}

impl ScheduledDispatch for NoQueueHandler {
    const DECLARED: bool = false;

    fn dispatch(&self, _tick: ScheduledTick) -> impl Future<Output = Result<(), BoxError>> + Send {
        // Unreachable in practice: `run` refuses a scheduled event before it gets this far when
        // `DECLARED` is false, the same as it does for SQS.
        core::future::ready(Err(BoxError::from(
            "this application declares no #[skyzen::scheduled] handler",
        )))
    }
}
