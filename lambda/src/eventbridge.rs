//! Decoding `EventBridge` scheduled events into the portable `ScheduledTick`.
//!
//! An `EventBridge` rule's `cron(...)` and an `EventBridge` Scheduler schedule both land here as
//! an invocation whose `detail-type` is `Scheduled Event` and whose `source` is `aws.events` or
//! `aws.scheduler`. Neither shape reports the cron that produced it — the rule's own name is the
//! closest thing — so the CLI also stamps the portable expression into `detail.cron` when it
//! provisions the trigger, and the tick's `cron` prefers that marker.
//!
//! The tick's `scheduled_time_ms` is the event's `time` field — the instant the schedule was
//! due, which is also what a retried or delayed invocation still names.

use std::time::{SystemTime, UNIX_EPOCH};

use lambda_runtime::Error;
use serde::Deserialize;
use skyzen_services::ScheduledTick;

use crate::ScheduledDispatch;

/// Just enough of an event to recognize a scheduled one.
#[derive(Debug, Deserialize)]
pub struct EventBridgeKind<'a> {
    /// `Scheduled Event` for every scheduled invocation.
    #[serde(default, rename = "detail-type", borrow)]
    pub detail_type: Option<&'a str>,
    /// `aws.events` for a rule's `cron(...)`, `aws.scheduler` for an `EventBridge` Scheduler
    /// schedule — either means the platform, not a custom event bus producer.
    #[serde(default, borrow)]
    pub source: Option<&'a str>,
}

/// Whether the event is a scheduled one.
#[must_use]
pub fn is_scheduled(kind: &EventBridgeKind<'_>) -> bool {
    kind.detail_type == Some("Scheduled Event")
        && matches!(kind.source, Some("aws.events" | "aws.scheduler"))
}

/// Turn the event payload into the portable tick the handler is given.
///
/// # Errors
///
/// Fails when the event carries neither a `detail.cron` marker nor a `resources` entry to name
/// the rule from — a scheduled event must carry one, so its absence means the payload was not
/// produced by the mechanism this decodes.
pub fn tick(event: &serde_json::Value) -> Result<ScheduledTick, Error> {
    let cron = event
        .get("detail")
        .and_then(|detail| detail.get("cron"))
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || rule_name(event).map(ToOwned::to_owned),
            |cron| Ok(cron.to_owned()),
        )?;

    let scheduled_time_ms = event
        .get("time")
        .and_then(serde_json::Value::as_str)
        .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
        .map_or_else(now_ms, |time| time.timestamp_millis());

    Ok(ScheduledTick::new(cron, scheduled_time_ms))
}

/// The rule's name, out of its ARN in `resources[0]`.
///
/// `EventBridge` ARNs look like `arn:aws:events:{region}:{account}:rule/{name}` — the name is
/// everything after the last slash, and it is what the CLI sets the rule's name to anyway.
fn rule_name(event: &serde_json::Value) -> Result<&str, Error> {
    event
        .get("resources")
        .and_then(serde_json::Value::as_array)
        .and_then(|resources| resources.first())
        .and_then(serde_json::Value::as_str)
        .and_then(|arn| arn.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            Error::from(
                "a scheduled event carries neither a `detail.cron` marker nor a rule ARN in \
                 `resources`; the handler has no cron to report",
            )
        })
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
pub async fn handle_scheduled<D: ScheduledDispatch>(
    event: &serde_json::Value,
    dispatch: &D,
) -> Result<serde_json::Value, Error> {
    if !D::DECLARED {
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

    let handled = std::panic::AssertUnwindSafe(dispatch.dispatch(tick.clone()));
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

/// Now, in epoch milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| {
            i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
        })
}
