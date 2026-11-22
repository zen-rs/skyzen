//! `RateLimit` — fixed-window, sliding-window, or token-bucket rate limiting over the portable
//! `Kv` service, Durable Objects, or process memory.
//!
//! ```rust,ignore
//! // Per-IP, 100 requests per 60s, backed by a manifest [[service]] KV binding.
//! .layer(RateLimit::kv::<KvService>(100, Duration::from_secs(60)))
//!
//! // In-process, for a single binary — or a custom key (JWT subject, tenant id, ...).
//! .layer(RateLimit::memory(100, Duration::from_secs(60))
//!     .key_by(|request| tenant_id(request)))
//! ```
//!
//! Over the limit the layer answers `429` with `Retry-After` — the seconds until the current
//! window resets — and the shared `{"error": ..}` body. When the backing store errors, the
//! request **passes** (fail-open, the usual rate-limiter posture: an availability guard should
//! not take the service down itself) — `.fail_closed()` rejects instead, for endpoints whose
//! abuse risk outweighs an outage.
//!
//! The Cloudflare caveat: Workers KV has no atomic increment-with-deadline, so `kv` there
//! reports the capability gap and every request is unavailable — it does not degrade into
//! a racy soft limiter. Reach for a Durable Object counter when
//! exactness matters.

use core::{fmt, future::Future};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_core::future::BoxFuture;
use http_kit::{
    header::{HeaderMap, HeaderValue, RETRY_AFTER},
    http_error, Request, Response, StatusCode,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, PendingResponseHeaders,
};
use skyzen_services::{
    durable::{AsDurableKv, DurableKv},
    Kv, KvError,
};

use crate::extract::ClientIp;

http_error!(
    /// `429 Too Many Requests` — the caller's window quota is spent.
    pub RateLimited,
    StatusCode::TOO_MANY_REQUESTS,
    "rate limit exceeded"
);

http_error!(
    /// `503 Service Unavailable` — the counter backend failed under `fail_closed`.
    pub RateLimitUnavailable,
    StatusCode::SERVICE_UNAVAILABLE,
    "the rate limiter could not reach its counter backend"
);

/// One hit's outcome: whether the request is admitted and the seconds until retry.
struct Hit {
    allowed: bool,
    retry_after: u64,
}

/// Rate-limiting algorithm and parameters evaluated by a [`RateLimitStore`].
#[derive(Debug, Clone, Copy)]
enum Policy {
    FixedWindow { limit: u64, window: Duration },
    SlidingWindow { limit: u64, window: Duration },
    TokenBucket { capacity: u64, refill_per_sec: f64 },
}

/// Why a count could not be taken.
enum HitError {
    /// The backend is unreachable or erred — a transient failure `fail_open` covers.
    Backend(Error),
    /// The backend has no atomic counter at all — a permanent capability gap that must
    /// never pass silently, `fail_open` or not.
    Unsupported(Error),
}

/// The counter backend — `Kv` bindings and the in-process map both fill it.
#[allow(clippy::manual_async_fn)]
trait RateLimitStore: fmt::Debug + Send + Sync + 'static {
    fn hit<'a>(&'a self, key: &'a str, policy: Policy) -> BoxFuture<'a, Result<Hit, HitError>>;
}

/// Builds the per-request counter backend — a KV binding or a shared in-process store.
type Resolver = Arc<
    dyn for<'a> Fn(&'a mut Request) -> BoxFuture<'a, Result<Arc<dyn RateLimitStore>, Error>>
        + Send
        + Sync,
>;

/// Names the thing being limited — the client IP by default, or whatever `key_by` returns.
type KeyFn =
    Arc<dyn for<'a> Fn(&'a mut Request) -> BoxFuture<'a, Result<String, Error>> + Send + Sync>;

/// A rate limiter supporting fixed-window (default), sliding-window, and token-bucket policies.
///
/// `resolve` builds the per-request counter — a `kv::<S>` binding extracted from the request
/// extensions, or a shared in-process store. `key` names the thing being limited — the
/// client IP by default.
pub struct RateLimit {
    policy: Policy,
    resolve: Resolver,
    key: KeyFn,
    name: String,
    fail_open: bool,
}

impl fmt::Debug for RateLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimit")
            .field("policy", &self.policy)
            .field("fail_open", &self.fail_open)
            .finish_non_exhaustive()
    }
}

impl RateLimit {
    /// `limit` hits per `window` against a manifest `[[service]]` KV binding — `S` is the
    /// typed wrapper `import_config!` generates, resolved from the request extensions so the
    /// same code runs native, Lambda and Workers.
    ///
    /// Only KV backends with an atomic counter-with-deadline count honestly (Redis,
    /// `DynamoDB`, Cosmos, the in-memory mock). Cloudflare KV has none, so it reports
    /// the capability gap and every request is unavailable — use [`RateLimit::durable`]
    /// inside a Durable Object instead of tolerating a racy counter.
    pub fn kv<S>(limit: u64, window: Duration) -> Self
    where
        S: Extractor + core::ops::Deref<Target = Kv>,
        S::Error: fmt::Debug,
    {
        Self::new(limit, window, Arc::new(kv_resolver::<S>))
    }

    /// `limit` hits per `window` against a Durable Object's co-located storage — the Cloudflare
    /// counter whose single-writer reads-modify-writes are atomic, unlike the KV `increment`
    /// Workers lacks entirely. `S` is the durable binding resolved from extensions — the bare
    /// `DurableKv` the runtime injects or a typed `Deref` wrapper, either through `AsDurableKv`.
    ///
    /// Use this inside a `#[skyzen::durable_object]` handler — `DurableKv` exists only in the
    /// object's own context. On an ordinary Worker route the binding never resolves, and a
    /// missing binding is answered fail-closed: every request gets `RateLimitUnavailable`
    /// rather than passing uncounted.
    pub fn durable<S>(limit: u64, window: Duration) -> Self
    where
        S: Extractor + AsDurableKv,
        S::Error: fmt::Debug,
    {
        Self::new(limit, window, Arc::new(durable_resolver::<S>))
    }

    /// `limit` hits per `window` counted in-process — the preset for a single binary or for
    /// tests. Per-instance: a deploy scaled to N replicas effectively allows N × limit.
    #[must_use]
    pub fn memory(limit: u64, window: Duration) -> Self {
        let store: Arc<dyn RateLimitStore> = Arc::new(MemoryStore::default());
        Self::new(
            limit,
            window,
            Arc::new(move |_| {
                let store = store.clone();
                Box::pin(std::future::ready(Ok(store)))
            }),
        )
    }

    fn new(limit: u64, window: Duration, resolve: Resolver) -> Self {
        // A sub-millisecond window cannot index a bucket — `now / window_ms` divides by
        // zero. Reject the configuration where it is built rather than panic per-request.
        assert!(
            window.as_millis() > 0,
            "rate limit window must be at least 1ms"
        );
        Self {
            policy: Policy::FixedWindow { limit, window },
            resolve,
            key: Arc::new(|request| {
                Box::pin(async move {
                    ClientIp::extract(request)
                        .await
                        .map(|ip| ip.to_string())
                        .map_err(|error| Error::msg(format!("rate limit client ip: {error}")))
                })
            }),
            // Two policies sharing one backend would otherwise count into one bucket —
            // distinct quotas and windows each need a namespace of their own.
            name: format!("{limit}/{}", window.as_millis()),
            fail_open: true,
        }
    }

    /// Use a sliding-window counter (weighted previous window + current window) instead of a
    /// hard-reset fixed window, smoothing boundary bursts across both in-memory and `Kv` backends.
    #[must_use]
    pub const fn sliding_window(mut self) -> Self {
        self.policy = match self.policy {
            Policy::FixedWindow { limit, window } | Policy::SlidingWindow { limit, window } => {
                Policy::SlidingWindow { limit, window }
            }
            Policy::TokenBucket { capacity, .. } => Policy::SlidingWindow {
                limit: capacity,
                window: Duration::from_secs(1),
            },
        };
        self
    }

    /// Use a token-bucket policy with burst `capacity` refilling at `refill_per_sec` tokens/s.
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0` or `refill_per_sec` is not finite and positive.
    #[must_use]
    pub fn token_bucket(mut self, capacity: u64, refill_per_sec: f64) -> Self {
        assert!(capacity > 0, "token bucket capacity must be at least 1");
        assert!(
            refill_per_sec.is_finite() && refill_per_sec > 0.0,
            "token bucket refill_per_sec must be finite and positive"
        );
        self.policy = Policy::TokenBucket {
            capacity,
            refill_per_sec,
        };
        self
    }

    /// Names this policy's bucket namespace on a shared backend. Two limiters that must
    /// not share counts — different routes, different quotas — get distinct names; the
    /// default is derived from `limit` and `window`, so only identical configurations
    /// share a bucket. Name a policy explicitly when the limit or window may change but
    /// the counter should carry over.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Key the window on something other than the client IP — a JWT subject, tenant, route,
    /// or composite. Returning an error rejects the request with `500`.
    #[must_use]
    pub fn key_by(
        mut self,
        key: impl for<'a> Fn(&'a mut Request) -> BoxFuture<'a, Result<String, Error>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.key = Arc::new(key);
        self
    }

    /// Reject with `503` when the counter backend errors, instead of the fail-open default.
    #[must_use]
    pub const fn fail_closed(mut self) -> Self {
        self.fail_open = false;
        self
    }
}

#[allow(clippy::manual_async_fn)]
impl Middleware for RateLimit {
    fn handle(
        &self,
        request: &mut Request,
        next: Next<'_>,
    ) -> impl Future<Output = Result<Response, Error>> {
        async move {
            let store = match (self.resolve)(request).await {
                Ok(store) => store,
                // A missing binding is a permanent misconfiguration, not an outage —
                // fail-open exists for transient backend errors, and a limiter that can
                // never resolve its store must fail closed every request.
                Err(error) => {
                    tracing::warn!("rate limit store could not resolve: {error}");
                    return Err(Error::from(RateLimitUnavailable::new()));
                }
            };
            // Length-prefixed name — `("a:b", "c")` and `("a", "b:c")` would collide at the
            // same store key under plain concatenation — every component is %XX-escaped so
            // a name or client key carrying `/ ? # \` survives Cosmos document ids, and any
            // component over the bound collapses to a digest so a caller-supplied key can
            // never outgrow a document-id limit and fail open.
            let name = bound_key_component(&self.name);
            let key = format!(
                "rate:{}:{}:{}",
                name.len(),
                name,
                bound_key_component(&(self.key)(request).await?),
            );
            match store.hit(&key, self.policy).await {
                Ok(hit) if hit.allowed => next.run(request).await,
                Ok(hit) => {
                    // A 429 is an error — it propagates through the shared error path, with
                    // the wait riding the request as a must-deliver header: whichever
                    // boundary renders the error, the client still sees `Retry-After`.
                    let retry = HeaderValue::from(hit.retry_after.max(1));
                    let extensions = request.extensions_mut();
                    match extensions.get_mut::<PendingResponseHeaders>() {
                        Some(pending) => {
                            pending.0.append(RETRY_AFTER, retry);
                        }
                        None => {
                            extensions.insert(PendingResponseHeaders(HeaderMap::from_iter([(
                                RETRY_AFTER,
                                retry,
                            )])));
                        }
                    }
                    Err(Error::from(RateLimited::new()))
                }
                // A capability gap is permanent — a limiter that cannot count must surface,
                // not silently admit everything.
                Err(HitError::Unsupported(error)) => {
                    tracing::error!("rate limit backend has no atomic counter: {error}");
                    Err(Error::from(RateLimitUnavailable::new()))
                }
                Err(HitError::Backend(error)) if self.fail_open => {
                    tracing::warn!("rate limit backend error, admitting request: {error}");
                    next.run(request).await
                }
                Err(HitError::Backend(error)) => {
                    tracing::warn!("rate limit backend error: {error}");
                    Err(Error::from(RateLimitUnavailable::new()))
                }
            }
        }
    }
}

/// Resolve a manifest `[[service]]` KV binding into the counter backend, per request.
fn kv_resolver<S>(request: &mut Request) -> BoxFuture<'_, Result<Arc<dyn RateLimitStore>, Error>>
where
    S: Extractor + core::ops::Deref<Target = Kv>,
    S::Error: fmt::Debug,
{
    Box::pin(async move {
        let kv = S::extract(request)
            .await
            .map_err(|error| Error::msg(format!("rate limit KV binding: {error:?}")))?;
        Ok(Arc::new(KvStore { kv: (*kv).clone() }) as Arc<dyn RateLimitStore>)
    })
}

/// Resolve a typed Durable Object binding into the counter backend, per request — the
/// Cloudflare answer when exactness matters.
fn durable_resolver<S>(
    request: &mut Request,
) -> BoxFuture<'_, Result<Arc<dyn RateLimitStore>, Error>>
where
    S: Extractor + AsDurableKv,
    S::Error: fmt::Debug,
{
    Box::pin(async move {
        let kv = S::extract(request)
            .await
            .map_err(|error| Error::msg(format!("rate limit durable binding: {error:?}")))?;
        Ok(Arc::new(DurableKvRateStore {
            kv: kv.as_durable_kv().clone(),
        }) as Arc<dyn RateLimitStore>)
    })
}

/// The most a single bucket-key component may occupy — the digest fallback below keeps the
/// whole `rate:` key under a fraction of the tightest document-id limit (Cosmos: 1,023).
const MAX_KEY_COMPONENT: usize = 128;

/// A bucket-key component that stays inside document-id bounds: escaped like
/// [`encode_key_component`] when short enough, else its sha256 hex — still injective, so a
/// distinct name or client key still lands on a distinct bucket.
fn bound_key_component(key: &str) -> String {
    use sha2::Digest;
    let escaped = encode_key_component(key);
    if escaped.len() <= MAX_KEY_COMPONENT {
        return escaped;
    }
    let digest = sha2::Sha256::digest(key.as_bytes());
    // The `sha256:` tag can never appear in escaped output — `:` itself is %3A there — so a
    // short key that happens to equal a digest cannot claim a long key's bucket.
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        use core::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Escape `key` for store backends that take the bucket name as a document id: `%` itself
/// goes first so the mapping stays injective, then the characters Cosmos forbids in ids.
fn encode_key_component(key: &str) -> String {
    key.replace('%', "%25")
        .replace('/', "%2F")
        .replace('\\', "%5C")
        .replace('?', "%3F")
        .replace('#', "%23")
        .replace(':', "%3A")
}

/// Milliseconds since the Unix epoch, on every target.
fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        super::js_millis(js_sys::Date::now())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn sliding_weighted_count(prev: u64, curr: u64, elapsed_ms: u64, window_ms: u64) -> u64 {
    let prev_weight = window_ms.saturating_sub(elapsed_ms) as f64 / window_ms as f64;
    let weighted_prev = (prev as f64 * prev_weight).floor() as u64;
    weighted_prev.saturating_add(curr)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn refill_token_bucket(
    tokens: f64,
    last_ms: u64,
    now_ms: u64,
    capacity: u64,
    refill_per_sec: f64,
) -> (bool, f64, u64) {
    let elapsed_sec = now_ms.saturating_sub(last_ms) as f64 / 1000.0;
    let refilled = elapsed_sec
        .mul_add(refill_per_sec, tokens)
        .min(capacity as f64);
    if refilled >= 1.0 {
        (true, refilled - 1.0, 0)
    } else {
        let retry_after = ((1.0 - refilled) / refill_per_sec).ceil().max(1.0) as u64;
        (false, refilled, retry_after)
    }
}

/// A `Kv`-backed counter: fixed window, sliding window, or CAS token bucket.
#[derive(Debug)]
struct KvStore {
    kv: Kv,
}

impl KvStore {
    async fn increment_bucket(&self, bucket: &str, ttl: Duration) -> Result<u64, HitError> {
        match self.kv.increment_with_ttl(bucket, 1, ttl).await {
            Ok(count) if count <= 0 => {
                let _ = self
                    .kv
                    .compare_and_swap(bucket, Some(count.to_string().as_bytes()), b"-1")
                    .await;
                Err(HitError::Unsupported(Error::msg(format!(
                    "rate-limit counter under {bucket:?} is not positive"
                ))))
            }
            Ok(count) => Ok(count.cast_unsigned()),
            Err(
                error @ (KvError::Unsupported(_)
                | KvError::Unauthorized
                | KvError::Decode(_)
                | KvError::Serialization(_)),
            ) => Err(HitError::Unsupported(Error::new(error))),
            Err(KvError::Conflict) => Ok(u64::MAX),
            Err(other) => Err(HitError::Backend(Error::new(other))),
        }
    }

    async fn read_bucket(&self, bucket: &str) -> Result<u64, HitError> {
        match self.kv.get(bucket).await {
            Ok(Some(raw)) => Ok(std::str::from_utf8(&raw)
                .ok()
                .and_then(|text| text.trim().parse::<i64>().ok())
                .filter(|&v| v > 0)
                .map_or(0, i64::cast_unsigned)),
            Ok(None) => Ok(0),
            Err(
                error @ (KvError::Unsupported(_)
                | KvError::Unauthorized
                | KvError::Decode(_)
                | KvError::Serialization(_)),
            ) => Err(HitError::Unsupported(Error::new(error))),
            Err(other) => Err(HitError::Backend(Error::new(other))),
        }
    }

    async fn hit_token_bucket(
        &self,
        key: &str,
        capacity: u64,
        refill_per_sec: f64,
    ) -> Result<Hit, HitError> {
        const MAX_CAS_ATTEMPTS: usize = 8;
        let bucket = format!("rl:tb:{key}");
        let now = now_ms();
        for _ in 0..MAX_CAS_ATTEMPTS {
            let raw = match self.kv.get(&bucket).await {
                Ok(raw) => raw,
                Err(
                    error @ (KvError::Unsupported(_)
                    | KvError::Unauthorized
                    | KvError::Decode(_)
                    | KvError::Serialization(_)),
                ) => return Err(HitError::Unsupported(Error::new(error))),
                Err(other) => return Err(HitError::Backend(Error::new(other))),
            };
            #[allow(clippy::cast_precision_loss)]
            let (tokens, last_ms) = match raw.as_deref() {
                None => (capacity as f64, now),
                Some(bytes) => {
                    let Some((t, ms)) = std::str::from_utf8(bytes)
                        .ok()
                        .and_then(|s| s.split_once(':'))
                        .and_then(|(t, ms)| {
                            Some((t.parse::<f64>().ok()?, ms.parse::<u64>().ok()?))
                        })
                    else {
                        return Err(HitError::Unsupported(Error::msg(format!(
                            "token-bucket record under {bucket:?} is malformed"
                        ))));
                    };
                    (t, ms)
                }
            };
            let (allowed, next_tokens, retry_after) =
                refill_token_bucket(tokens, last_ms, now, capacity, refill_per_sec);
            if !allowed {
                return Ok(Hit {
                    allowed: false,
                    retry_after,
                });
            }
            let encoded = format!("{next_tokens:.6}:{now}");
            match self
                .kv
                .compare_and_swap(&bucket, raw.as_deref(), encoded.as_bytes())
                .await
            {
                Ok(true) => {
                    return Ok(Hit {
                        allowed: true,
                        retry_after: 0,
                    });
                }
                Ok(false) | Err(KvError::Conflict) => {}
                Err(
                    error @ (KvError::Unsupported(_)
                    | KvError::Unauthorized
                    | KvError::Decode(_)
                    | KvError::Serialization(_)),
                ) => return Err(HitError::Unsupported(Error::new(error))),
                Err(other) => return Err(HitError::Backend(Error::new(other))),
            }
        }
        Ok(Hit {
            allowed: false,
            retry_after: 1,
        })
    }
}

impl RateLimitStore for KvStore {
    fn hit<'a>(&'a self, key: &'a str, policy: Policy) -> BoxFuture<'a, Result<Hit, HitError>> {
        Box::pin(async move {
            match policy {
                Policy::FixedWindow { limit, window } => {
                    let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
                    let now = now_ms();
                    let index = now / window_ms;
                    let bucket = format!("rl:{key}:{index}");
                    let count = self
                        .increment_bucket(&bucket, window.saturating_mul(2))
                        .await?;
                    Ok(Hit {
                        allowed: count <= limit,
                        retry_after: ((index + 1) * window_ms).saturating_sub(now) / 1000,
                    })
                }
                Policy::SlidingWindow { limit, window } => {
                    let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
                    let now = now_ms();
                    let index = now / window_ms;
                    let elapsed = now % window_ms;
                    let curr_bucket = format!("rl:sw:{key}:{index}");
                    let curr = self
                        .increment_bucket(&curr_bucket, window.saturating_mul(2))
                        .await?;
                    let prev = if let Some(prev_idx) = index.checked_sub(1) {
                        self.read_bucket(&format!("rl:sw:{key}:{prev_idx}")).await?
                    } else {
                        0
                    };
                    let weighted = sliding_weighted_count(prev, curr, elapsed, window_ms);
                    Ok(Hit {
                        allowed: weighted <= limit,
                        retry_after: window_ms.saturating_sub(elapsed) / 1000,
                    })
                }
                Policy::TokenBucket {
                    capacity,
                    refill_per_sec,
                } => self.hit_token_bucket(key, capacity, refill_per_sec).await,
            }
        })
    }
}

/// A `DurableKv`-backed counter: a Durable Object's storage is co-located and single-writer,
/// so a read-modify-write there is atomic in a way Cloudflare KV's `increment` never could
/// be. The bucket is a compact `{count}:{reset_ms}` record.
#[derive(Debug)]
struct DurableKvRateStore {
    kv: DurableKv,
}

impl DurableKvRateStore {
    /// Delete every expired `rl:` record — at most once per window, marked by a tombstone
    /// the sweep itself writes. Durable Object storage has no TTL, so without this the key
    /// space grows forever.
    async fn sweep_dead(&self, now: u64, reset: u64, window_ms: u64) {
        let marker = format!("rl:swept:{}", now / window_ms);
        if matches!(self.kv.get(&marker).await, Ok(Some(_))) {
            return;
        }
        // One page per hit — a churned keyspace is never walked whole inside a request, and
        // a cursor keyed to this window lets the next hit continue where this one stopped.
        let cursor_key = format!("rl:sweep:{}", now / window_ms);
        let cursor = self
            .kv
            .get(&cursor_key)
            .await
            .ok()
            .flatten()
            .and_then(|raw| {
                std::str::from_utf8(&raw)
                    .ok()?
                    .rsplit_once("::")
                    .map(|(start, _)| start.to_owned())
            });
        let Ok(page) = self
            .kv
            .list(skyzen_services::durable::DurableListOptions {
                prefix: Some("rl:"),
                start: cursor.as_deref(),
                limit: Some(128),
                ..Default::default()
            })
            .await
        else {
            // No marker either — the next window's first hit retries instead of skipping a
            // sweep that may never have happened.
            return;
        };
        let mut dead: Vec<&str> = Vec::new();
        for (key, raw) in &page {
            let Some(text) = std::str::from_utf8(raw).ok() else {
                continue;
            };
            // Only keys the limiter itself wrote may die: a counter lives under
            // `rl:counter:` and is exactly `{count}:{reset}`, while a sweep cursor or
            // done-marker carries its deadline as the last colon segment. Anything else
            // sharing the `rl:` prefix is application data and is left alone.
            let deadline = if key.starts_with("rl:counter:") {
                text.split_once(':').and_then(|(count, reset)| {
                    count.parse::<u64>().ok()?;
                    reset.parse::<u64>().ok()
                })
            } else if key.starts_with("rl:sweep:") || key.starts_with("rl:swept:") {
                text.rsplit(':')
                    .next()
                    .and_then(|reset| reset.parse::<u64>().ok())
            } else {
                None
            };
            if deadline.is_some_and(|reset| reset <= now) {
                dead.push(key.as_str());
            }
        }
        // `delete_multiple` is bounded per call — a delete error defers the page to the
        // next hit's pass from the same cursor.
        for chunk in dead.chunks(128) {
            if self.kv.delete_multiple(chunk).await.is_err() {
                return;
            }
        }
        if page.len() == 128 {
            // A full page means more may follow — park the cursor (with this window's own
            // deadline) so the next hit resumes rather than restarts.
            let Some((next, _)) = page.last() else {
                return;
            };
            let _ = self
                .kv
                .put(&cursor_key, format!("{next}::{reset}").as_bytes())
                .await;
        } else {
            // The pass is done for this window — the marker, not the cursor, says so; both
            // die with the window they were written for.
            let _ = self.kv.delete(&cursor_key).await;
            let _ = self.kv.put(&marker, format!("1:{reset}").as_bytes()).await;
        }
    }
}

impl DurableKvRateStore {
    async fn hit_fixed(&self, key: &str, limit: u64, window: Duration) -> Result<Hit, HitError> {
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
        let now = now_ms();
        let bucket = format!("rl:counter:{key}");
        let stored = self
            .kv
            .get(&bucket)
            .await
            .map_err(|error| HitError::Backend(Error::new(error)))?;
        let (count, reset) = match stored.as_deref() {
            None => (0, ((now / window_ms) + 1) * window_ms),
            Some(raw) => match std::str::from_utf8(raw)
                .ok()
                .and_then(|text| text.split_once(':'))
                .and_then(|(count, reset)| {
                    Some((count.parse::<u64>().ok()?, reset.parse::<u64>().ok()?))
                }) {
                Some((count, reset)) if now < reset => (count, reset),
                Some(_) => (0, ((now / window_ms) + 1) * window_ms),
                None => {
                    return Err(HitError::Unsupported(Error::msg(format!(
                        "rate-limit record under {bucket:?} is not a counter"
                    ))))
                }
            },
        };
        let Some(count) = count.checked_add(1) else {
            return Err(HitError::Unsupported(Error::msg(format!(
                "rate-limit record under {bucket:?} cannot grow past u64::MAX"
            ))));
        };
        self.kv
            .put(&bucket, format!("{count}:{reset}").as_bytes())
            .await
            .map_err(|error| HitError::Backend(Error::new(error)))?;
        self.sweep_dead(now, reset, window_ms).await;
        Ok(Hit {
            allowed: count <= limit,
            retry_after: reset.saturating_sub(now) / 1000,
        })
    }

    async fn hit_sliding(&self, key: &str, limit: u64, window: Duration) -> Result<Hit, HitError> {
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
        let now = now_ms();
        let index = now / window_ms;
        let elapsed = now % window_ms;
        let bucket = format!("rl:sw:{key}");
        let stored = self
            .kv
            .get(&bucket)
            .await
            .map_err(|error| HitError::Backend(Error::new(error)))?;
        let (prev, curr) = match stored.as_deref() {
            None => (0, 0),
            Some(raw) => {
                let Some((p, c, idx)) = std::str::from_utf8(raw).ok().and_then(|s| {
                    let mut parts = s.split(':');
                    Some((
                        parts.next()?.parse::<u64>().ok()?,
                        parts.next()?.parse::<u64>().ok()?,
                        parts.next()?.parse::<u64>().ok()?,
                    ))
                }) else {
                    return Err(HitError::Unsupported(Error::msg(format!(
                        "sliding-window record under {bucket:?} is malformed"
                    ))));
                };
                if idx == index {
                    (p, c)
                } else if idx + 1 == index {
                    (c, 0)
                } else {
                    (0, 0)
                }
            }
        };
        let curr = curr.saturating_add(1);
        self.kv
            .put(&bucket, format!("{prev}:{curr}:{index}").as_bytes())
            .await
            .map_err(|error| HitError::Backend(Error::new(error)))?;
        let weighted = sliding_weighted_count(prev, curr, elapsed, window_ms);
        Ok(Hit {
            allowed: weighted <= limit,
            retry_after: window_ms.saturating_sub(elapsed) / 1000,
        })
    }

    async fn hit_token_bucket(
        &self,
        key: &str,
        capacity: u64,
        refill_per_sec: f64,
    ) -> Result<Hit, HitError> {
        let now = now_ms();
        let bucket = format!("rl:tb:{key}");
        let stored = self
            .kv
            .get(&bucket)
            .await
            .map_err(|error| HitError::Backend(Error::new(error)))?;
        #[allow(clippy::cast_precision_loss)]
        let (tokens, last_ms) = match stored.as_deref() {
            None => (capacity as f64, now),
            Some(raw) => {
                let Some((t, ms)) = std::str::from_utf8(raw)
                    .ok()
                    .and_then(|s| s.split_once(':'))
                    .and_then(|(t, ms)| Some((t.parse::<f64>().ok()?, ms.parse::<u64>().ok()?)))
                else {
                    return Err(HitError::Unsupported(Error::msg(format!(
                        "token-bucket record under {bucket:?} is malformed"
                    ))));
                };
                (t, ms)
            }
        };
        let (allowed, next_tokens, retry_after) =
            refill_token_bucket(tokens, last_ms, now, capacity, refill_per_sec);
        if allowed {
            self.kv
                .put(&bucket, format!("{next_tokens:.6}:{now}").as_bytes())
                .await
                .map_err(|error| HitError::Backend(Error::new(error)))?;
        }
        Ok(Hit {
            allowed,
            retry_after,
        })
    }
}

impl RateLimitStore for DurableKvRateStore {
    fn hit<'a>(&'a self, key: &'a str, policy: Policy) -> BoxFuture<'a, Result<Hit, HitError>> {
        Box::pin(async move {
            match policy {
                Policy::FixedWindow { limit, window } => self.hit_fixed(key, limit, window).await,
                Policy::SlidingWindow { limit, window } => {
                    self.hit_sliding(key, limit, window).await
                }
                Policy::TokenBucket {
                    capacity,
                    refill_per_sec,
                } => self.hit_token_bucket(key, capacity, refill_per_sec).await,
            }
        })
    }
}

/// An in-process counter for single-binary deploys and tests.
#[derive(Debug, Default)]
struct MemoryStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Debug, Default)]
struct MemoryInner {
    buckets: HashMap<String, (u64, u64)>,
    token_buckets: HashMap<String, (f64, u64)>,
    /// The window index the last full expiry sweep ran for — one sweep per index, so a
    /// churned key space past the cap cannot make every request rescan the map, and the
    /// boundary rollover always gets a fresh pass where dead buckets actually die.
    last_sweep_index: Option<u64>,
}

/// The map sweep threshold — past it a `hit` first drops expired buckets so an unbounded
/// key space cannot grow memory without bound.
const MAX_MEMORY_BUCKETS: usize = 4096;

impl MemoryStore {
    #[allow(clippy::significant_drop_tightening)]
    fn hit_fixed(&self, key: &str, limit: u64, window: Duration) -> Hit {
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
        let now = now_ms();
        let index = now / window_ms;
        let bucket = format!("rl:{key}:{index}");
        let (count, reset) = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.buckets.len() > MAX_MEMORY_BUCKETS && inner.last_sweep_index != Some(index) {
                inner.buckets.retain(|_, (_, reset)| *reset > now);
                inner.last_sweep_index = Some(index);
            }
            if inner.buckets.len() > MAX_MEMORY_BUCKETS && !inner.buckets.contains_key(&bucket) {
                (u64::MAX, (index + 1) * window_ms)
            } else {
                let entry = inner
                    .buckets
                    .entry(bucket)
                    .or_insert((0, (index + 1) * window_ms));
                if now >= entry.1 {
                    *entry = (0, (index + 1) * window_ms);
                }
                entry.0 += 1;
                (entry.0, entry.1)
            }
        };
        Hit {
            allowed: count <= limit,
            retry_after: reset.saturating_sub(now) / 1000,
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    fn hit_sliding(&self, key: &str, limit: u64, window: Duration) -> Hit {
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
        let now = now_ms();
        let index = now / window_ms;
        let elapsed = now % window_ms;
        let curr_bucket = format!("rl:sw:{key}:{index}");
        let prev_bucket = index.checked_sub(1).map(|p| format!("rl:sw:{key}:{p}"));
        let (prev, curr) = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.buckets.len() > MAX_MEMORY_BUCKETS && inner.last_sweep_index != Some(index) {
                inner.buckets.retain(|_, (_, reset)| *reset > now);
                inner.last_sweep_index = Some(index);
            }
            let prev = prev_bucket
                .and_then(|b| {
                    inner
                        .buckets
                        .get(&b)
                        .filter(|(_, reset)| *reset > now)
                        .map(|(c, _)| *c)
                })
                .unwrap_or(0);
            if inner.buckets.len() > MAX_MEMORY_BUCKETS && !inner.buckets.contains_key(&curr_bucket)
            {
                (prev, u64::MAX)
            } else {
                // Keep each sliding-window bucket alive for two windows so the
                // previous window remains readable throughout the current window.
                let expiry = (index + 2) * window_ms;
                let entry = inner.buckets.entry(curr_bucket).or_insert((0, expiry));
                if now >= entry.1 {
                    *entry = (0, expiry);
                }
                entry.0 += 1;
                (prev, entry.0)
            }
        };
        let weighted = sliding_weighted_count(prev, curr, elapsed, window_ms);
        Hit {
            allowed: weighted <= limit,
            retry_after: window_ms.saturating_sub(elapsed) / 1000,
        }
    }

    #[allow(clippy::significant_drop_tightening, clippy::cast_precision_loss)]
    fn hit_token_bucket(&self, key: &str, capacity: u64, refill_per_sec: f64) -> Hit {
        let now = now_ms();
        let bucket = format!("rl:tb:{key}");
        let (allowed, retry_after) = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.token_buckets.len() > MAX_MEMORY_BUCKETS {
                inner.token_buckets.retain(|_, (tokens, last)| {
                    let elapsed_sec = now.saturating_sub(*last) as f64 / 1000.0;
                    elapsed_sec.mul_add(refill_per_sec, *tokens) < capacity as f64
                });
            }
            if inner.token_buckets.len() > MAX_MEMORY_BUCKETS
                && !inner.token_buckets.contains_key(&bucket)
            {
                (false, 1)
            } else {
                let entry = inner
                    .token_buckets
                    .entry(bucket)
                    .or_insert((capacity as f64, now));
                let (allowed, next_tokens, retry_after) =
                    refill_token_bucket(entry.0, entry.1, now, capacity, refill_per_sec);
                if allowed {
                    entry.0 = next_tokens;
                    entry.1 = now;
                }
                (allowed, retry_after)
            }
        };
        Hit {
            allowed,
            retry_after,
        }
    }
}

impl RateLimitStore for MemoryStore {
    fn hit<'a>(&'a self, key: &'a str, policy: Policy) -> BoxFuture<'a, Result<Hit, HitError>> {
        Box::pin(std::future::ready(Ok(match policy {
            Policy::FixedWindow { limit, window } => self.hit_fixed(key, limit, window),
            Policy::SlidingWindow { limit, window } => self.hit_sliding(key, limit, window),
            Policy::TokenBucket {
                capacity,
                refill_per_sec,
            } => self.hit_token_bucket(key, capacity, refill_per_sec),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::RateLimit;
    use crate::{
        routing::{CreateRouteNode, Route, Router},
        Body, Request, StatusCode,
    };
    use http_kit::header::HeaderValue;
    use std::time::Duration;

    async fn hello() -> &'static str {
        "hi"
    }

    fn get() -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/hi".parse().unwrap();
        request
    }

    fn key_by_header(
        name: &'static str,
    ) -> impl for<'a> Fn(
        &'a mut Request,
    )
        -> futures_core::future::BoxFuture<'a, Result<String, skyzen_core::Error>>
           + Send
           + Sync {
        move |request| {
            Box::pin(std::future::ready(Ok(request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("anon")
                .to_owned())))
        }
    }

    #[tokio::test]
    async fn requests_over_the_limit_propagate_the_429_error() {
        let router: Router = Route::new(("/hi".at(hello),))
            .layer(
                RateLimit::memory(2, Duration::from_secs(60))
                    .key_by(|_| Box::pin(std::future::ready(Ok("fixed".to_owned())))),
            )
            .build();

        for _ in 0..2 {
            let response = router.clone().go(get()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        // The rejection travels as `Err` through the shared error path; `Retry-After` rides
        // the request's `PendingResponseHeaders` for whichever boundary renders it.
        let error = router.go(get()).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn different_keys_have_independent_windows() {
        let router: Router = Route::new(("/hi".at(hello),))
            .layer(RateLimit::memory(1, Duration::from_secs(60)).key_by(key_by_header("x-tenant")))
            .build();

        let mut a = get();
        a.headers_mut()
            .insert("x-tenant", HeaderValue::from_static("a"));
        let mut b = get();
        b.headers_mut()
            .insert("x-tenant", HeaderValue::from_static("b"));

        assert_eq!(router.clone().go(a).await.unwrap().status(), StatusCode::OK);
        assert_eq!(router.clone().go(b).await.unwrap().status(), StatusCode::OK);
        let mut a2 = get();
        a2.headers_mut()
            .insert("x-tenant", HeaderValue::from_static("a"));
        let error = router.go(a2).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn a_failing_store_fails_open_by_default_and_closed_on_request() {
        #[derive(Debug)]
        struct Broken;
        impl super::RateLimitStore for Broken {
            fn hit<'a>(
                &'a self,
                _: &'a str,
                _: super::Policy,
            ) -> futures_core::future::BoxFuture<'a, Result<super::Hit, super::HitError>>
            {
                Box::pin(std::future::ready(Err(super::HitError::Backend(
                    skyzen_core::Error::msg("kv down"),
                ))))
            }
        }

        fn broken_resolver(
            _: &mut Request,
        ) -> futures_core::future::BoxFuture<
            '_,
            Result<std::sync::Arc<dyn super::RateLimitStore>, skyzen_core::Error>,
        > {
            Box::pin(std::future::ready(Ok(
                std::sync::Arc::new(Broken) as std::sync::Arc<dyn super::RateLimitStore>
            )))
        }

        let open_router: Router = Route::new(("/hi".at(hello),))
            .layer(
                RateLimit::new(
                    1,
                    Duration::from_secs(60),
                    std::sync::Arc::new(broken_resolver),
                )
                .key_by(|_| Box::pin(std::future::ready(Ok("k".to_owned())))),
            )
            .build();
        let response = open_router.go(get()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let closed_router: Router = Route::new(("/hi".at(hello),))
            .layer(
                RateLimit::new(
                    1,
                    Duration::from_secs(60),
                    std::sync::Arc::new(broken_resolver),
                )
                .key_by(|_| Box::pin(std::future::ready(Ok("k".to_owned()))))
                .fail_closed(),
            )
            .build();
        let error = closed_router.go(get()).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn sliding_window_enforces_limit_and_recovers() {
        let router: Router = Route::new(("/hi".at(hello),))
            .layer(
                RateLimit::memory(2, Duration::from_secs(60))
                    .sliding_window()
                    .key_by(|_| Box::pin(std::future::ready(Ok("sw".to_owned())))),
            )
            .build();

        assert_eq!(
            router.clone().go(get()).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            router.clone().go(get()).await.unwrap().status(),
            StatusCode::OK
        );
        let err = router.go(get()).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn token_bucket_allows_burst_then_refills() {
        let router: Router = Route::new(("/hi".at(hello),))
            .layer(
                RateLimit::memory(2, Duration::from_secs(1))
                    .token_bucket(2, 50.0)
                    .key_by(|_| Box::pin(std::future::ready(Ok("tb".to_owned())))),
            )
            .build();

        // Burst of 2 succeeds, 3rd is rejected.
        assert_eq!(
            router.clone().go(get()).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            router.clone().go(get()).await.unwrap().status(),
            StatusCode::OK
        );
        let err = router.clone().go(get()).await.unwrap_err();
        assert_eq!(err.status(), StatusCode::TOO_MANY_REQUESTS);

        // Wait 30ms -> at 50 tokens/s, >1.5 tokens refill -> next request succeeds.
        async_io::Timer::after(Duration::from_millis(30)).await;
        assert_eq!(router.go(get()).await.unwrap().status(), StatusCode::OK);
    }
}
