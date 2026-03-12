//! Durable Object alarm scheduling abstraction.

use core::{convert::Infallible, future::Future};

use http_kit::{
    http_error,
    middleware::{Middleware, MiddlewareError},
    Endpoint, Response,
};
use skyzen_core::{Extractor, StatusCode};

use crate::maybe_send::{BoxFuture, MaybeSend};

// ── Error type ──

/// Errors from Durable Object alarm operations.
#[derive(Debug, thiserror::Error)]
pub enum AlarmError {
    /// The underlying storage backend returned an error.
    #[error("alarm error: {0}")]
    Backend(String),
}

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

impl Clone for Alarm {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for Alarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Alarm").finish_non_exhaustive()
    }
}

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

http_error!(
    /// The Alarm service was not found in request extensions.
    pub AlarmNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Alarm scheduler not configured. Ensure an AlarmScheduler implementation is injected."
);

impl Extractor for Alarm {
    type Error = AlarmNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(AlarmNotConfigured::new())
    }
}

impl Middleware for Alarm {
    type Error = Infallible;

    async fn handle<N: Endpoint>(
        &mut self,
        request: &mut http_kit::Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        request.extensions_mut().insert(self.clone());
        next.respond(request)
            .await
            .map_err(MiddlewareError::Endpoint)
    }
}
