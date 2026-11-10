//! Decoding `EventBridge` scheduled events into the portable `ScheduledTick`.
//!
//! An `EventBridge` rule's `cron(...)` lands here as an invocation whose `detail-type` is
//! `Scheduled Event` and whose `source` is `aws.events`. The platform's own event reports
//! neither the cron nor the schedule's name, so the CLI stamps the portable expression into
//! `detail.cron` when it provisions the rule — and that marker is *required*: a payload that
//! lacks it did not come through this mechanism, and reporting a rule's name in `cron`'s place
//! would silently violate what the field promises.
//!
//! The tick's `scheduled_time_ms` is the event's `time` field — the instant the schedule was
//! due, which is also what a retried or delayed invocation still names. A missing or malformed
//! `time` is refused for the same reason: inventing "now" would report an instant the schedule
//! never meant.

use lambda_runtime::Error;
use serde::Deserialize;
use skyzen_services::ScheduledTick;

use crate::QueueDispatch;

/// Just enough of an event to recognize a scheduled one.
#[derive(Debug, Deserialize)]
pub struct EventBridgeKind<'a> {
    /// `Scheduled Event` for every scheduled invocation.
    #[serde(default, rename = "detail-type", borrow)]
    pub detail_type: Option<&'a str>,
    /// `aws.events` for a rule's `cron(...)`; `aws.scheduler` marks an `EventBridge` Scheduler
    /// schedule, whose payload this module does not decode.
    #[serde(default, borrow)]
    pub source: Option<&'a str>,
}

/// Whether the event is a scheduled one this module decodes.
#[must_use]
pub fn is_scheduled(kind: &EventBridgeKind<'_>) -> bool {
    kind.detail_type == Some("Scheduled Event") && kind.source == Some("aws.events")
}

/// Whether the event is an `EventBridge` Scheduler invocation — a scheduled shape this module
/// deliberately does not decode, worth refusing by name rather than misreading as HTTP.
#[must_use]
pub fn is_scheduler(kind: &EventBridgeKind<'_>) -> bool {
    kind.detail_type == Some("Scheduled Event") && kind.source == Some("aws.scheduler")
}

/// Turn the event payload into the portable tick the handler is given.
///
/// # Errors
///
/// Fails when `detail.cron` is absent — the marker the provisioned rule always stamps, without
/// which the handler's `cron` has no honest value — or when `time` is missing or malformed,
/// since the tick's `scheduled_time_ms` must be the instant the schedule was due.
pub fn tick(event: &serde_json::Value) -> Result<ScheduledTick, Error> {
    let cron = event
        .get("detail")
        .and_then(|detail| detail.get("cron"))
        .and_then(serde_json::Value::as_str)
        .filter(|cron| !cron.is_empty())
        .ok_or_else(|| {
            Error::from(
                "a scheduled event carries no `detail.cron` marker; the rule was not provisioned \
                 by `skyzen deploy`, which stamps the portable cron there",
            )
        })?
        .to_owned();

    let scheduled_time_ms = event
        .get("time")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::from("a scheduled event carries no `time` field"))
        .and_then(|time| {
            chrono::DateTime::parse_from_rfc3339(time)
                .map(|parsed| parsed.timestamp_millis())
                .map_err(|error| {
                    Error::from(format!(
                        "a scheduled event's `time` is not RFC 3339: {error}"
                    ))
                })
        })?;

    Ok(ScheduledTick::new(cron, scheduled_time_ms))
}

/// Drive the application's scheduled handler for one pushed tick.
///
/// A handler that fails or panics fails the invocation: `EventBridge` retries failed invocations
/// on its own schedule, the same way it retries any failed scheduled target.
///
/// # Errors
///
/// [`Error`] when the application declares no `#[skyzen::scheduled]` handler, when the event is
/// not a decodable scheduled one, or when the handler itself fails or panics.
pub async fn handle_scheduled<D: QueueDispatch>(
    event: &serde_json::Value,
    dispatch: &D,
) -> Result<serde_json::Value, Error> {
    if !D::SCHEDULED_DECLARED {
        return Err(Error::from(
            "this Lambda received a scheduled event, but the application declares no \
             #[skyzen::scheduled] handler; annotate one, or point the EventBridge rule at a \
             function that has one",
        ));
    }

    let tick = tick(event)?;
    tracing::debug!(
        cron = tick.cron.as_str(),
        "handling a pushed scheduled tick"
    );

    let handled = std::panic::AssertUnwindSafe(dispatch.dispatch_scheduled(tick.clone()));
    futures_util::FutureExt::catch_unwind(handled)
        .await
        .map_or_else(
            |panic| {
                tracing::error!(
                    cron = tick.cron.as_str(),
                    panic = skyzen_core::panic_message(panic.as_ref()),
                    "scheduled handler panicked"
                );
                Err(Error::from("the scheduled handler panicked"))
            },
            |result| {
                result.inspect_err(|error| {
                    tracing::error!(
                        cron = tick.cron.as_str(),
                        error = %skyzen_core::ErrorChain(error.as_ref()),
                        "scheduled handler failed"
                    );
                })?;
                Ok(serde_json::Value::Null)
            },
        )
}
