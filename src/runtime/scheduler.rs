//! The native scheduler runtime.
//!
//! Cloudflare fires a Worker's `scheduled` export itself: the platform owns the timer. Everywhere
//! else something else does — Skyzen's own loop natively, EventBridge on Lambda, a `timerTrigger`
//! on Azure — and this module is the native half of that picture, which is what makes
//! `#[skyzen::scheduled]` dual-target the way `#[skyzen::queue]` already is.
//!
//! # Semantics
//!
//! Each `[[schedule]]` entry runs its own loop, sequentially: a handler that is still running
//! when its next tick comes due *delays* that tick rather than overlapping it. Ticks missed
//! while the handler ran are skipped, not queued for catch-up — the next fire is always the
//! first occurrence strictly after now, which matches Cloudflare's own at-most-once shape. A
//! tick is identified by the wall-clock instant it was scheduled for (`scheduled_time_ms`), not
//! by the time the loop happened to wake.
//!
//! Scheduling is per-process: N replicas of the application mean N independent timers and N
//! firings per tick. There is no distributed lock — a schedule that must not multiply belongs
//! behind a singleton of the application's own, exactly as a cron daemon on N hosts would be.

use core::{convert::Infallible, future::Future, time::Duration};
use std::{
    panic::AssertUnwindSafe,
    time::{SystemTime, UNIX_EPOCH},
};

use async_channel::{Receiver, Sender};
use chrono::Utc;
use executor_core::{Executor as CoreExecutor, Task};
use futures_util::FutureExt as _;
use skyzen_core::ErrorChain;
use skyzen_services::{BoxError, ScheduledHandler, ScheduledTick};
use tracing::{debug, error, info, warn};

use super::consumer::{interruptible, panic_message, yield_now};

/// One `[[schedule]]` entry, as the runtime sees it.
///
/// `#[skyzen::main]` builds one of these per manifest entry; the cron text is already validated
/// at compile time, so the loop's one `expect` is an invariant, not a check.
#[derive(Debug, Clone)]
pub struct ScheduleConfig {
    /// The `[[schedule]]` name, for logs.
    pub name: String,
    /// The portable 5-field cron expression, reported to the handler as
    /// [`ScheduledTick::cron`].
    pub cron: String,
}

/// The schedules an application declares, ready for the runtime to start.
///
/// A crate has at most one `#[skyzen::scheduled]` handler — the Cloudflare export name is
/// singular — so every entry shares one handler and the driver stays generic over its type
/// rather than boxing it.
pub struct Schedules<H> {
    handler: H,
    entries: Vec<ScheduleConfig>,
}

impl<H> core::fmt::Debug for Schedules<H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Schedules")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl<H: ScheduledHandler> Schedules<H> {
    /// Declare `entries`, all driven by `handler`.
    #[must_use]
    pub const fn new(handler: H, entries: Vec<ScheduleConfig>) -> Self {
        Self { handler, entries }
    }
}

/// The schedules an application runs beside its HTTP server.
///
/// Implemented by `()` for an application that declares none and by [`Schedules`] for one that
/// does, so the runtime has a single launch path and no "no schedule" placeholder handler.
///
/// # Being woken and being pushed
///
/// [`start`](Self::start) is the pull side: it owns the timer loops, and only a plain server
/// runs them — under Lambda or the Functions host the platform owns delivery, the same rule the
/// queue consumers follow. [`dispatch`](Self::dispatch) is the push side, for a platform that
/// invokes the application with a tick already in hand. [`fire_once`](Self::fire_once) is the
/// dev aid: it dispatches every declared schedule immediately, the native equivalent of
/// `wrangler dev --test-scheduled`. Both sides drive the same `#[skyzen::scheduled]` handler.
pub trait ScheduleSet: Send + Sync + 'static {
    /// Whether a `#[skyzen::scheduled]` handler exists at all.
    ///
    /// A const, so a platform integration can refuse a timer trigger *at startup* rather than on
    /// the first tick that arrives with nothing to handle it.
    const DECLARES_HANDLER: bool;

    /// Spawn every declared schedule loop.
    ///
    /// `stop` is closed when the process begins shutting down and `guard` is a drain token: each
    /// loop holds a clone until it has finished its current tick, which is what makes graceful
    /// shutdown wait for in-flight work.
    fn start<Exec: CoreExecutor + 'static>(
        self,
        executor: &Exec,
        stop: &Receiver<Infallible>,
        guard: &Sender<Infallible>,
    );

    /// Dispatch every declared schedule immediately, as a dev aid.
    ///
    /// Each entry's handler runs once under `guard`, with `scheduled_time_ms` set to the moment
    /// of firing — `skyzen dev -- --test-scheduled` reaches here through
    /// [`LaunchOptions::test_scheduled`](crate::runtime::native::LaunchOptions).
    fn fire_once<Exec: CoreExecutor + 'static>(&self, executor: &Exec, guard: &Sender<Infallible>);

    /// Hand a tick the platform pushed to the declared handler.
    ///
    /// # Errors
    ///
    /// Returns the handler's own error; the platform decides what a failed invocation means.
    fn dispatch(&self, tick: ScheduledTick) -> impl Future<Output = Result<(), BoxError>> + Send;
}

impl ScheduleSet for () {
    const DECLARES_HANDLER: bool = false;

    fn start<Exec: CoreExecutor + 'static>(
        self,
        _executor: &Exec,
        _stop: &Receiver<Infallible>,
        _guard: &Sender<Infallible>,
    ) {
    }

    fn fire_once<Exec: CoreExecutor + 'static>(
        &self,
        _executor: &Exec,
        _guard: &Sender<Infallible>,
    ) {
    }

    fn dispatch(&self, _tick: ScheduledTick) -> impl Future<Output = Result<(), BoxError>> + Send {
        // Callers check `DECLARES_HANDLER` first and refuse the trigger by name, so this is the
        // backstop for a hand-written integration that forgot to.
        core::future::ready(Err(BoxError::from(
            "this application declares no #[skyzen::scheduled] handler",
        )))
    }
}

impl<H: ScheduledHandler> ScheduleSet for Schedules<H> {
    const DECLARES_HANDLER: bool = true;

    fn start<Exec: CoreExecutor + 'static>(
        self,
        executor: &Exec,
        stop: &Receiver<Infallible>,
        guard: &Sender<Infallible>,
    ) {
        for config in self.entries {
            info!(
                schedule = config.name.as_str(),
                cron = config.cron.as_str(),
                "starting native schedule"
            );
            spawn_loop(
                executor,
                config,
                self.handler.clone(),
                stop.clone(),
                guard.clone(),
            );
        }
    }

    fn fire_once<Exec: CoreExecutor + 'static>(&self, executor: &Exec, guard: &Sender<Infallible>) {
        for config in &self.entries {
            info!(
                schedule = config.name.as_str(),
                cron = config.cron.as_str(),
                "firing schedule on demand (--test-scheduled)"
            );
            let handler = self.handler.clone();
            let tick = ScheduledTick::new(config.cron.clone(), now_ms());
            let name = config.name.clone();
            let guard = guard.clone();
            executor
                .spawn(async move {
                    // Held for as long as the fired tick runs, so a shutdown waits for the
                    // handler this asked to run.
                    let _guard = guard;
                    fire(&handler, name, tick).await;
                })
                .detach();
        }
    }

    fn dispatch(&self, tick: ScheduledTick) -> impl Future<Output = Result<(), BoxError>> + Send {
        self.handler.handle(tick)
    }
}

/// Spawn one schedule's timer loop, holding `guard` until the loop exits.
fn spawn_loop<Exec: CoreExecutor + 'static, H: ScheduledHandler>(
    executor: &Exec,
    config: ScheduleConfig,
    handler: H,
    stop: Receiver<Infallible>,
    guard: Sender<Infallible>,
) {
    executor
        .spawn(async move {
            // Held for as long as the loop runs, so a shutdown waits for the tick this slot is
            // in the middle of firing.
            let _guard = guard;
            run_schedule(config, handler, stop).await;
        })
        .detach();
}

/// Drive `handler` on `config`'s cron until `stop` is closed.
///
/// One call is one `[[schedule]]` entry: ticks are sequential, and a handler still running when
/// its next tick comes due delays it — the next fire is computed *after* the handler returns,
/// so missed wall-clock ticks are skipped rather than queued for catch-up.
///
/// # Panics
///
/// The cron was validated when the manifest parsed, so a `ScheduleConfig` a macro built always
/// parses; a hand-built one with an expression [`saffron`] refuses panics here, on startup —
/// before a loop that could silently never fire gets to run.
pub async fn run_schedule<H: ScheduledHandler>(
    config: ScheduleConfig,
    handler: H,
    stop: Receiver<Infallible>,
) {
    let cron = config
        .cron
        .parse::<saffron::Cron>()
        .unwrap_or_else(|error| {
            panic!(
                "schedule `{}` has an invalid cron expression `{}`: {error}",
                config.name, config.cron
            )
        });

    loop {
        // Parity with the consumer loop: every await in an iteration can be ready on creation,
        // so without an explicit yield a schedule that always has work due would starve the
        // other tasks sharing this executor thread.
        yield_now().await;

        if stop.is_closed() {
            break;
        }

        let Some(next) = cron.next_after(Utc::now()) else {
            // `CronExpr` rejects expressions that can never match, so `None` here is only a
            // hand-built `ScheduleConfig` — say so rather than idling forever.
            warn!(
                schedule = config.name.as_str(),
                cron = config.cron.as_str(),
                "schedule can never fire; stopping its loop"
            );
            break;
        };
        let scheduled_ms = next.timestamp_millis();
        let delay = (next - Utc::now()).to_std().unwrap_or(Duration::ZERO);

        if interruptible(&stop, async_io::Timer::after(delay))
            .await
            .is_none()
        {
            break;
        }

        fire(
            &handler,
            config.name.clone(),
            ScheduledTick::new(config.cron.clone(), scheduled_ms),
        )
        .await;
    }

    debug!(schedule = config.name.as_str(), "native schedule stopped");
}

/// Hand one tick to the handler, catching a panic the way the consumer driver does.
async fn fire<H: ScheduledHandler>(handler: &H, name: String, tick: ScheduledTick) {
    debug!(
        schedule = name.as_str(),
        cron = tick.cron.as_str(),
        "firing schedule"
    );
    match AssertUnwindSafe(handler.handle(tick)).catch_unwind().await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => error!(
            schedule = name.as_str(),
            error = %ErrorChain(error.as_ref()),
            "scheduled handler failed"
        ),
        Err(panic) => error!(
            schedule = name.as_str(),
            panic = panic_message(panic.as_ref()),
            "scheduled handler panicked"
        ),
    }
}

/// Now, in epoch milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| {
            i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
        })
}
