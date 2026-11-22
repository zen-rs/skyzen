//! Multi-upstream target selection, health tracking, and retry policy for [`Proxy`](super::Proxy).

use std::{
    cell::Cell,
    pin::pin,
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use executor_core::{LocalExecutor, Task as _};
use futures_util::future::{join_all, select};
use http::uri::{Authority, PathAndQuery, Scheme};
use zenwave::{backend::NativeBackend, Timeouts};

use super::{ProxyError, Upstream, UpstreamError};
use crate::{Body, Endpoint as _, Request, Uri};

/// A single validated upstream origin and its optional base path.
#[derive(Debug, Clone)]
pub(super) struct Target {
    pub(super) scheme: Scheme,
    pub(super) authority: Authority,
    /// The upstream path prefix every forwarded path is appended to, without a trailing `/`.
    pub(super) base: String,
}

impl Target {
    /// Validate and parse `uri` into an upstream target.
    pub(super) fn parse(uri: &Uri) -> Result<Self, UpstreamError> {
        let scheme = match uri.scheme() {
            Some(scheme) if *scheme == Scheme::HTTP || *scheme == Scheme::HTTPS => scheme.clone(),
            _ => return Err(UpstreamError::Scheme(uri.to_string())),
        };
        let Some(authority) = uri.authority().cloned() else {
            return Err(UpstreamError::MissingAuthority(uri.to_string()));
        };
        if uri.query().is_some() {
            return Err(UpstreamError::Query(uri.to_string()));
        }
        Ok(Self {
            scheme,
            authority,
            base: uri.path().trim_end_matches('/').to_owned(),
        })
    }

    /// Build the upstream URI for a forwarded request's `path_and_query`.
    pub(super) fn request_uri(
        &self,
        path_and_query: Option<&PathAndQuery>,
    ) -> Result<Uri, ProxyError> {
        let path = path_and_query.map_or("/", PathAndQuery::as_str);
        let mut joined = String::with_capacity(self.base.len() + path.len());
        joined.push_str(&self.base);
        joined.push_str(path);
        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(joined)
            .build()
            .map_err(|error| ProxyError::Target(error.to_string()))
    }

    /// Build the URI for an active health-check probe at `path`.
    fn probe_uri(&self, path: &str) -> Result<Uri, ProxyError> {
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(path)
            .build()
            .map_err(|error| ProxyError::Target(error.to_string()))
    }
}

/// How [`Proxy`](super::Proxy) distributes requests across multiple upstream origins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LoadBalance {
    /// Distribute requests sequentially across healthy origins.
    #[default]
    RoundRobin,
    /// Route each request to the healthy origin with the fewest in-flight requests.
    LeastConnections,
}

/// Passive health tracking (outlier ejection) for upstream origins.
///
/// When an origin fails `consecutive_failures` times in a row, it is marked unhealthy and skipped
/// for `ejection_duration` before being tried again. If all origins in the pool are ejected, the
/// proxy fails open and tries the next origin rather than rejecting every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassiveHealth {
    consecutive_failures: u32,
    ejection_duration: Duration,
    count_5xx: bool,
}

impl PassiveHealth {
    /// Eject an origin for `ejection_duration` after `consecutive_failures` consecutive failures.
    ///
    /// By default, transport errors, timeouts, and `5xx` responses count as failures.
    #[must_use]
    pub fn new(consecutive_failures: u32, ejection_duration: Duration) -> Self {
        Self {
            consecutive_failures: consecutive_failures.max(1),
            ejection_duration,
            count_5xx: true,
        }
    }

    /// Whether `5xx` responses from the origin count toward consecutive failures (`true` by default).
    #[must_use]
    pub const fn count_5xx(mut self, count: bool) -> Self {
        self.count_5xx = count;
        self
    }

    /// Whether `status` should be recorded as a passive failure.
    pub(super) fn is_failure_status(self, status: http::StatusCode) -> bool {
        self.count_5xx && status.is_server_error()
    }
}

impl Default for PassiveHealth {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(10))
    }
}

/// Active HTTP health probing for upstream origins.
///
/// Periodically sends a `GET` request to `path` on each origin using the proxy's own client.
/// Origins that fail `unhealthy_threshold` consecutive probes are marked unhealthy until
/// `healthy_threshold` consecutive probes return a `2xx` status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    path: String,
    interval: Duration,
    timeout: Duration,
    unhealthy_threshold: u32,
    healthy_threshold: u32,
}

impl HealthCheck {
    /// Probe each origin with `GET path` using default timings (10s interval, 2s timeout, threshold 1).
    #[must_use]
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(2),
            unhealthy_threshold: 1,
            healthy_threshold: 1,
        }
    }

    /// Set how often probes run (default: 10 seconds).
    #[must_use]
    pub const fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Set the per-probe connect and response-head timeout (default: 2 seconds).
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set how many consecutive failed probes mark an origin unhealthy (default: 1).
    #[must_use]
    pub fn unhealthy_threshold(mut self, count: u32) -> Self {
        self.unhealthy_threshold = count.max(1);
        self
    }

    /// Set how many consecutive `2xx` probes restore an unhealthy origin (default: 1).
    #[must_use]
    pub fn healthy_threshold(mut self, count: u32) -> Self {
        self.healthy_threshold = count.max(1);
        self
    }
}

/// Retry policy for idempotent requests that fail before a response is committed.
///
/// Only requests with an idempotent HTTP method (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`,
/// `TRACE`), no upgrade offer, and an empty body (`Body::empty()` / known zero length) are ever
/// retried. Streaming or non-empty request bodies and `POST`/`PATCH` requests are sent at most
/// once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: usize,
    retry_on_timeout: bool,
    retry_on_gateway_errors: bool,
}

impl RetryPolicy {
    /// Retry idempotent empty-body requests up to `max_attempts` total attempts across origins.
    ///
    /// By default, transport errors and upstream timeouts are retried; `502`/`503`/`504` responses
    /// are retried when [`RetryPolicy::on_gateway_errors`] is enabled.
    #[must_use]
    pub fn idempotent(max_attempts: usize) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            retry_on_timeout: true,
            retry_on_gateway_errors: false,
        }
    }

    /// Whether upstream timeouts (`504 Gateway Timeout`) trigger a retry (`true` by default).
    #[must_use]
    pub const fn on_timeout(mut self, retry: bool) -> Self {
        self.retry_on_timeout = retry;
        self
    }

    /// Whether `502`, `503`, and `504` responses from an origin trigger a retry (`false` by default).
    #[must_use]
    pub const fn on_gateway_errors(mut self, retry: bool) -> Self {
        self.retry_on_gateway_errors = retry;
        self
    }

    /// Maximum total attempts allowed for an eligible request.
    pub(super) const fn max_attempts(self) -> usize {
        self.max_attempts
    }

    /// Whether a timeout error should be retried.
    pub(super) const fn retries_timeout(self) -> bool {
        self.retry_on_timeout
    }

    /// Whether an upstream `status` should be retried.
    pub(super) const fn retries_status(self, status: http::StatusCode) -> bool {
        self.retry_on_gateway_errors
            && matches!(
                status,
                http::StatusCode::BAD_GATEWAY
                    | http::StatusCode::SERVICE_UNAVAILABLE
                    | http::StatusCode::GATEWAY_TIMEOUT
            )
    }
}

/// Per-target mutable state on a single serving thread.
#[derive(Debug)]
struct TargetState {
    in_flight: Arc<AtomicUsize>,
    consecutive_failures: Cell<u32>,
    ejected_until: Cell<Option<Instant>>,
    active_healthy: Cell<bool>,
    active_successes: Cell<u32>,
    active_failures: Cell<u32>,
}

impl TargetState {
    fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicUsize::new(0)),
            consecutive_failures: Cell::new(0),
            ejected_until: Cell::new(None),
            active_healthy: Cell::new(true),
            active_successes: Cell::new(0),
            active_failures: Cell::new(0),
        }
    }

    fn is_healthy(&self, now: Instant) -> bool {
        if !self.active_healthy.get() {
            return false;
        }
        if let Some(until) = self.ejected_until.get() {
            if now < until {
                return false;
            }
            // Cooldown expired: allow a half-open probe request.
            self.ejected_until.set(None);
        }
        true
    }
}

/// Per-thread load-balancing and health state shared by all clones of a [`Proxy`](super::Proxy).
pub(super) struct PoolState {
    targets: Vec<TargetState>,
    cursor: Cell<usize>,
    /// Dropping this channel closes the active health-check loop when the last `Proxy` clone drops.
    _stop_probes: Option<async_channel::Sender<()>>,
}

impl std::fmt::Debug for PoolState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolState")
            .field("targets", &self.targets.len())
            .finish_non_exhaustive()
    }
}

impl PoolState {
    /// Create per-thread state for `upstream` and spawn active health checks on `executor` if configured.
    pub(super) fn new<E: LocalExecutor>(
        upstream: &Arc<Upstream>,
        client: &NativeBackend,
        executor: &E,
    ) -> Rc<Self> {
        let targets = (0..upstream.targets.len())
            .map(|_| TargetState::new())
            .collect();
        let (stop_tx, stop_rx) = if upstream.health_check.is_some() {
            let (tx, rx) = async_channel::bounded(1);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let state = Rc::new(Self {
            targets,
            cursor: Cell::new(0),
            _stop_probes: stop_tx,
        });
        if let (Some(check), Some(stop_rx)) = (upstream.health_check.clone(), stop_rx) {
            let weak = Rc::downgrade(&state);
            let upstream = Arc::clone(upstream);
            let client = client.clone();
            executor
                .spawn_local(probe_loop(weak, upstream, client, check, stop_rx))
                .detach();
        }
        state
    }

    /// Select the next target index, preferring healthy untried targets and failing open to any
    /// untried target if all are currently unhealthy.
    pub(super) fn select(&self, strategy: LoadBalance, tried: &[bool]) -> Option<usize> {
        let now = Instant::now();
        self.pick(strategy, |idx| {
            !tried[idx] && self.targets[idx].is_healthy(now)
        })
        .or_else(|| self.pick(strategy, |idx| !tried[idx]))
    }

    fn pick(&self, strategy: LoadBalance, eligible: impl Fn(usize) -> bool) -> Option<usize> {
        let len = self.targets.len();
        let start = self.cursor.get() % len;
        let chosen = match strategy {
            LoadBalance::RoundRobin => (0..len)
                .map(|offset| (start + offset) % len)
                .find(|&idx| eligible(idx)),
            LoadBalance::LeastConnections => (0..len)
                .map(|offset| (start + offset) % len)
                .filter(|&idx| eligible(idx))
                .min_by_key(|&idx| self.targets[idx].in_flight.load(Ordering::Relaxed)),
        };
        if let Some(idx) = chosen {
            self.cursor.set((idx + 1) % len);
        }
        chosen
    }

    /// Acquire an in-flight guard for `idx`.
    pub(super) fn acquire(&self, idx: usize) -> InFlightGuard {
        let counter = Arc::clone(&self.targets[idx].in_flight);
        counter.fetch_add(1, Ordering::Relaxed);
        InFlightGuard { counter }
    }

    /// Record a successful response from `idx`.
    pub(super) fn record_success(&self, idx: usize) {
        let target = &self.targets[idx];
        target.consecutive_failures.set(0);
        target.ejected_until.set(None);
    }

    /// Record a failure on `idx` against `passive` health settings.
    pub(super) fn record_failure(&self, idx: usize, passive: Option<PassiveHealth>) {
        let Some(passive) = passive else {
            return;
        };
        let target = &self.targets[idx];
        let failures = target.consecutive_failures.get().saturating_add(1);
        target.consecutive_failures.set(failures);
        if failures >= passive.consecutive_failures {
            target
                .ejected_until
                .set(Some(Instant::now() + passive.ejection_duration));
        }
    }

    /// Apply the result of one active health-check probe on `idx`.
    fn record_probe(&self, idx: usize, ok: bool, check: &HealthCheck) {
        let target = &self.targets[idx];
        if ok {
            target.active_failures.set(0);
            let successes = target.active_successes.get().saturating_add(1);
            target.active_successes.set(successes);
            if successes >= check.healthy_threshold {
                target.active_healthy.set(true);
                target.consecutive_failures.set(0);
                target.ejected_until.set(None);
            }
        } else {
            target.active_successes.set(0);
            let failures = target.active_failures.get().saturating_add(1);
            target.active_failures.set(failures);
            if failures >= check.unhealthy_threshold {
                target.active_healthy.set(false);
            }
        }
    }
}

/// RAII guard decrementing `in_flight` for one target when dropped.
pub(super) struct InFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Periodically probe every target until the owning [`PoolState`] is dropped.
async fn probe_loop(
    weak: Weak<PoolState>,
    upstream: Arc<Upstream>,
    client: NativeBackend,
    check: HealthCheck,
    stop: async_channel::Receiver<()>,
) {
    loop {
        if weak.upgrade().is_none() {
            return;
        }
        let probes = upstream.targets.iter().enumerate().map(|(idx, target)| {
            let mut client = client.clone();
            let check = &check;
            async move {
                let Ok(uri) = target.probe_uri(&check.path) else {
                    return (idx, false);
                };
                let mut req = Request::new(Body::empty());
                *req.method_mut() = http::Method::GET;
                *req.uri_mut() = uri;
                *req.version_mut() = http::Version::HTTP_11;
                req.extensions_mut().insert(
                    Timeouts::default()
                        .with_connect(check.timeout)
                        .with_response_head(check.timeout),
                );
                let ok =
                    matches!(client.respond(&mut req).await, Ok(resp) if resp.status().is_success());
                (idx, ok)
            }
        });
        let results = join_all(probes).await;
        let Some(state) = weak.upgrade() else {
            return;
        };
        for (idx, ok) in results {
            state.record_probe(idx, ok, &check);
        }
        drop(state);
        if matches!(
            select(
                pin!(async_io::Timer::after(check.interval)),
                pin!(stop.recv())
            )
            .await,
            futures_util::future::Either::Right(_)
        ) {
            return;
        }
    }
}
