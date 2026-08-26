//! In-memory alarm scheduler for testing.

use core::future::{ready, Future};
use std::sync::{Arc, RwLock};

use skyzen_services::durable::alarm::{AlarmError, AlarmScheduler};

/// In-memory implementation of [`AlarmScheduler`] for testing.
#[derive(Debug, Clone, Default)]
pub struct InMemoryAlarm {
    scheduled: Arc<RwLock<Option<i64>>>,
}

impl InMemoryAlarm {
    /// Create a new in-memory alarm scheduler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check the currently scheduled alarm time (for test assertions).
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[must_use]
    pub fn scheduled_time(&self) -> Option<i64> {
        *self.scheduled.read().expect("lock poisoned")
    }
}

impl InMemoryAlarm {
    fn store(&self, scheduled_time_ms: Option<i64>) -> Result<(), AlarmError> {
        self.scheduled
            .write()
            .map_err(|_| AlarmError::backend("lock poisoned"))
            .map(|mut scheduled| *scheduled = scheduled_time_ms)
    }
}

// A single `Option` behind a lock answers every call synchronously, so each future is ready on
// creation rather than an `async` block with nothing to await.
impl AlarmScheduler for InMemoryAlarm {
    fn get_alarm(&self) -> impl Future<Output = Result<Option<i64>, AlarmError>> + Send {
        ready(
            self.scheduled
                .read()
                .map_err(|_| AlarmError::backend("lock poisoned"))
                .map(|scheduled| *scheduled),
        )
    }

    fn set_alarm(
        &self,
        scheduled_time_ms: i64,
    ) -> impl Future<Output = Result<(), AlarmError>> + Send {
        ready(self.store(Some(scheduled_time_ms)))
    }

    fn delete_alarm(&self) -> impl Future<Output = Result<(), AlarmError>> + Send {
        ready(self.store(None))
    }
}
