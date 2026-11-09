//! Portable event payloads shared across runtimes.

use core::future::Future;

use crate::BoxError;

/// A portable scheduled trigger payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTick {
    /// The cron expression that triggered the event.
    pub cron: String,
    /// The scheduled time in milliseconds since the Unix epoch.
    pub scheduled_time_ms: i64,
}

impl ScheduledTick {
    /// Create a new scheduled tick.
    #[must_use]
    pub const fn new(cron: String, scheduled_time_ms: i64) -> Self {
        Self {
            cron,
            scheduled_time_ms,
        }
    }
}

/// A `#[skyzen::scheduled]` handler, as every non-Cloudflare target drives it.
///
/// On Cloudflare the platform calls the exported `scheduled` entrypoint directly; everywhere
/// else — Skyzen's own timer loop natively, `EventBridge` on Lambda, a `timerTrigger` on Azure —
/// something else owns delivery and this trait is what it calls. `#[skyzen::scheduled]`
/// generates the implementation; the trait lives here rather than in the runtime so
/// `skyzen-test` can drive the same generated handler without depending on `skyzen`.
pub trait ScheduledHandler: Clone + Send + Sync + 'static {
    /// Handle one scheduled tick.
    ///
    /// # Errors
    ///
    /// Returns the handler's own error. What a failure means is the platform's call: the native
    /// loop logs it and waits for the next tick, Azure reports a failed invocation, Lambda
    /// retries per the rule's policy.
    fn handle(&self, tick: ScheduledTick) -> impl Future<Output = Result<(), BoxError>> + Send;
}

/// Convert a `#[skyzen::scheduled]` handler's return value into the driver's result.
///
/// Mirrors `IntoWorkerResult` on Cloudflare: `()` and `Ok(())` report a successful tick, an
/// `Err` reports a failed one.
pub trait IntoScheduledOutcome {
    /// Convert the handler output into the result the driver reports.
    ///
    /// # Errors
    ///
    /// Returns the handler's error when it returned one.
    fn into_scheduled_outcome(self) -> Result<(), BoxError>;
}

impl IntoScheduledOutcome for () {
    fn into_scheduled_outcome(self) -> Result<(), BoxError> {
        Ok(())
    }
}

impl<E> IntoScheduledOutcome for Result<(), E>
where
    // `Into<BoxError>` is the bound every error type meets — `std::error::Error`
    // implementations through the blanket `From`, and `anyhow::Error` through its own
    // conversion, which `std::error::Error` deliberately does not cover.
    E: Into<BoxError>,
{
    fn into_scheduled_outcome(self) -> Result<(), BoxError> {
        self.map_err(Into::into)
    }
}
