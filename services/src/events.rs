//! Portable event payloads shared across runtimes.

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
