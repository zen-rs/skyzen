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
