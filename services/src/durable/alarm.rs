//! Durable Object alarm scheduling abstraction.

use core::future::Future;

use crate::maybe_send::{BoxFuture, MaybeSend};

// ── Error type ──

/// Errors from Durable Object alarm operations.
#[derive(Debug, thiserror::Error)]
pub enum AlarmError {
    /// The underlying storage backend returned an error.
    #[error("alarm error: {message}")]
    Backend {
        /// A human-readable description of what the backend was asked to do.
        message: String,
        /// The backend's own error, when it hands one back.
        #[source]
        source: Option<crate::BoxError>,
    },
}

backend_error!(AlarmError);

service_http_error!(AlarmError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
});

// ── Layer 1: Public trait ──

/// Durable Object alarm scheduling.
///
/// Allows setting, getting, and deleting a scheduled alarm
/// that will trigger the Durable Object's alarm handler.
pub trait AlarmScheduler: Send + Sync + Clone + 'static {
    /// Get the currently scheduled alarm time (ms since epoch), if any.
    fn get_alarm(&self) -> impl Future<Output = Result<Option<i64>, AlarmError>> + MaybeSend;

    /// Schedule an alarm at the given time (ms since epoch).
    fn set_alarm(
        &self,
        scheduled_time_ms: i64,
    ) -> impl Future<Output = Result<(), AlarmError>> + MaybeSend;

    /// Delete the currently scheduled alarm.
    fn delete_alarm(&self) -> impl Future<Output = Result<(), AlarmError>> + MaybeSend;
}

// ── Layer 2: Private object-safe trait ──

trait AlarmSchedulerObj: Send + Sync {
    fn get_alarm(&self) -> BoxFuture<'_, Result<Option<i64>, AlarmError>>;
    fn set_alarm(&self, scheduled_time_ms: i64) -> BoxFuture<'_, Result<(), AlarmError>>;
    fn delete_alarm(&self) -> BoxFuture<'_, Result<(), AlarmError>>;
    fn clone_box(&self) -> Box<dyn AlarmSchedulerObj>;
}

// ── Bridge ──

impl<T: AlarmScheduler> AlarmSchedulerObj for T {
    fn get_alarm(&self) -> BoxFuture<'_, Result<Option<i64>, AlarmError>> {
        Box::pin(AlarmScheduler::get_alarm(self))
    }
    fn set_alarm(&self, scheduled_time_ms: i64) -> BoxFuture<'_, Result<(), AlarmError>> {
        Box::pin(AlarmScheduler::set_alarm(self, scheduled_time_ms))
    }
    fn delete_alarm(&self) -> BoxFuture<'_, Result<(), AlarmError>> {
        Box::pin(AlarmScheduler::delete_alarm(self))
    }
    fn clone_box(&self) -> Box<dyn AlarmSchedulerObj> {
        Box::new(self.clone())
    }
}

// ── User-facing wrapper ──

/// Type-erased alarm scheduler extractor.
///
/// Wraps any [`AlarmScheduler`] behind dynamic dispatch.
pub struct Alarm(Box<dyn AlarmSchedulerObj>);

service_extractor!(
    Alarm,
    AlarmNotConfigured,
    "Alarm scheduler not configured. Ensure an AlarmScheduler implementation is injected."
);

impl Alarm {
    /// Create a new `Alarm` from any [`AlarmScheduler`] implementation.
    pub fn new(scheduler: impl AlarmScheduler) -> Self {
        Self(Box::new(scheduler))
    }

    /// Get the currently scheduled alarm time (ms since epoch), if any.
    ///
    /// # Errors
    ///
    /// Returns [`AlarmError`] if the backend operation fails.
    pub async fn get_alarm(&self) -> Result<Option<i64>, AlarmError> {
        self.0.get_alarm().await
    }

    /// Schedule an alarm at the given time (ms since epoch).
    ///
    /// # Errors
    ///
    /// Returns [`AlarmError`] if the backend operation fails.
    pub async fn set_alarm(&self, scheduled_time_ms: i64) -> Result<(), AlarmError> {
        self.0.set_alarm(scheduled_time_ms).await
    }

    /// Delete the currently scheduled alarm.
    ///
    /// # Errors
    ///
    /// Returns [`AlarmError`] if the backend operation fails.
    pub async fn delete_alarm(&self) -> Result<(), AlarmError> {
        self.0.delete_alarm().await
    }
}

#[cfg(test)]
mod tests {
    use super::{Alarm, AlarmError, AlarmNotConfigured, AlarmScheduler};
    use http_kit::{Body, Endpoint, HttpError, Middleware, Response};
    use skyzen_core::Extractor;
    use std::{
        convert::Infallible,
        sync::{Arc, RwLock},
    };

    #[derive(Clone, Default)]
    struct InMemoryAlarmScheduler {
        scheduled: Arc<RwLock<Option<i64>>>,
    }

    impl AlarmScheduler for InMemoryAlarmScheduler {
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

    #[derive(Debug)]
    struct ReadAlarmEndpoint;

    impl Endpoint for ReadAlarmEndpoint {
        type Error = Infallible;

        async fn respond(
            &mut self,
            request: &mut http_kit::Request,
        ) -> Result<Response, Self::Error> {
            let alarm = Alarm::extract(request)
                .await
                .expect("alarm should be injected");
            let scheduled = alarm
                .get_alarm()
                .await
                .expect("alarm access should succeed")
                .expect("alarm should exist");
            Ok(Response::new(Body::from(scheduled.to_string())))
        }
    }

    #[tokio::test]
    async fn wrapper_supports_set_get_and_delete_alarm() {
        let alarm = Alarm::new(InMemoryAlarmScheduler::default());

        assert_eq!(alarm.get_alarm().await.unwrap(), None);
        alarm.set_alarm(42).await.unwrap();
        assert_eq!(alarm.get_alarm().await.unwrap(), Some(42));
        alarm.delete_alarm().await.unwrap();
        assert_eq!(alarm.get_alarm().await.unwrap(), None);
    }

    #[tokio::test]
    async fn middleware_injects_alarm_for_downstream_endpoint_and_extractor() {
        let scheduler = InMemoryAlarmScheduler::default();
        scheduler.set_alarm(1337).await.unwrap();
        let mut alarm = Alarm::new(scheduler);
        let mut request = http_kit::Request::new(Body::empty());

        let response = alarm.handle(&mut request, ReadAlarmEndpoint).await.unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "1337");

        let extracted = Alarm::extract(&mut request).await.unwrap();
        assert_eq!(extracted.get_alarm().await.unwrap(), Some(1337));
    }

    #[tokio::test]
    async fn extractor_returns_internal_server_error_when_alarm_is_missing() {
        let mut request = http_kit::Request::new(Body::empty());

        let error = Alarm::extract(&mut request).await.unwrap_err();

        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn missing_configuration_error_uses_expected_status() {
        let error = AlarmNotConfigured::new();
        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
