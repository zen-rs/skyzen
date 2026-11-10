//! Helpers for constructing portable event payloads in tests.

use skyzen_services::{QueueBatch, QueueMessage, ScheduledTick};

/// Build a portable queue message for tests.
#[must_use]
pub fn queue_message<T>(id: impl Into<String>, timestamp_ms: i64, body: T) -> QueueMessage<T> {
    QueueMessage {
        id: id.into(),
        timestamp_ms,
        body,
    }
}

/// Build a portable queue batch for tests.
#[must_use]
pub fn queue_batch<T>(
    queue: impl Into<String>,
    messages: impl IntoIterator<Item = QueueMessage<T>>,
) -> QueueBatch<T> {
    QueueBatch {
        queue: queue.into(),
        messages: messages.into_iter().collect(),
    }
}

/// Build a portable scheduled tick for tests.
#[must_use]
pub fn scheduled_tick(cron: impl Into<String>, scheduled_time_ms: i64) -> ScheduledTick {
    ScheduledTick::new(cron.into(), scheduled_time_ms)
}

/// Drive a `#[skyzen::scheduled]` handler once, the way the platform does.
///
/// The handler is whatever `#[skyzen::scheduled]` generated `__SkyzenNativeScheduledHandler`
/// into — pass it by name; the same type the native loop and the serverless dispatchers call.
///
/// # Errors
///
/// [`FireScheduledError::Handler`] when the handler returns `Err`, and
/// [`FireScheduledError::Panic`] when it panics — the same two outcomes the runtime reports,
/// rather than propagating either.
///
/// # Example
///
/// ```ignore
/// use skyzen_test::{fire_scheduled, scheduled_tick};
///
/// let tick = scheduled_tick("*/10 * * * *", 1_735_689_600_000);
/// fire_scheduled(&__SkyzenNativeScheduledHandler, tick).await.expect("the tick is handled");
/// ```
pub async fn fire_scheduled<H: skyzen_services::ScheduledHandler>(
    handler: &H,
    tick: ScheduledTick,
) -> Result<(), FireScheduledError> {
    let outcome =
        futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(handler.handle(tick)))
            .await;
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(FireScheduledError::Handler(error)),
        Err(payload) => Err(FireScheduledError::Panic(payload)),
    }
}

/// What `fire_scheduled` reports when the handler does not succeed.
#[derive(Debug)]
pub enum FireScheduledError {
    /// The handler returned `Err` — its own error, boxed the way the platform reports it.
    Handler(skyzen_services::BoxError),
    /// The handler panicked, carrying the panic payload for whatever a test wants to assert on.
    Panic(Box<dyn core::any::Any + Send>),
}

impl core::fmt::Display for FireScheduledError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Handler(error) => write!(f, "the scheduled handler failed: {error}"),
            Self::Panic(payload) => write!(
                f,
                "the scheduled handler panicked: {}",
                skyzen_core::panic_message(payload.as_ref())
            ),
        }
    }
}

impl core::error::Error for FireScheduledError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Handler(error) => Some(error.as_ref()),
            Self::Panic(..) => None,
        }
    }
}
