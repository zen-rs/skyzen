//! In-memory alarm scheduler for testing.

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

impl AlarmScheduler for InMemoryAlarm {
    async fn get_alarm(&self) -> Result<Option<i64>, AlarmError> {
        let scheduled = self
            .scheduled
            .read()
            .map_err(|_| AlarmError::backend("lock poisoned"))?;
        Ok(*scheduled)
    }

    async fn set_alarm(&self, scheduled_time_ms: i64) -> Result<(), AlarmError> {
        *self
            .scheduled
            .write()
            .map_err(|_| AlarmError::backend("lock poisoned"))? = Some(scheduled_time_ms);
        Ok(())
    }

    async fn delete_alarm(&self) -> Result<(), AlarmError> {
        *self
            .scheduled
            .write()
            .map_err(|_| AlarmError::backend("lock poisoned"))? = None;
        Ok(())
    }
}
