//! `RateLimit` — a fixed-window counter over the portable `Kv` service, or process memory.
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
//! The Cloudflare caveat: Workers KV has no atomic increment, so `kv` there counts racy —
//! honest for a soft limit, not for a hard quota. Reach for a Durable Object counter when
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
use skyzen_services::{Kv, KvError};

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

/// One hit's outcome: the running count in this window and the seconds until it resets.
struct Hit {
    count: u64,
    retry_after: u64,
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
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit, HitError>>;
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

/// A fixed-window rate limiter.
///
/// `resolve` builds the per-request counter — a `kv::<S>` binding extracted from the request
/// extensions, or a shared in-process store. `key` names the thing being limited — the
/// client IP by default.
pub struct RateLimit {
    limit: u64,
    window: Duration,
    resolve: Resolver,
    key: KeyFn,
    name: String,
    fail_open: bool,
}

impl fmt::Debug for RateLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimit")
            .field("limit", &self.limit)
            .field("window", &self.window)
            .field("fail_open", &self.fail_open)
            .finish_non_exhaustive()
    }
}

impl RateLimit {
    /// `limit` hits per `window` against a manifest `[[service]]` KV binding — `S` is the
    /// typed wrapper `import_config!` generates, resolved from the request extensions so the
    /// same code runs native, Lambda and Workers.
    ///
    /// Only KV backends with an atomic `increment` count honestly (Redis, `DynamoDB`, Cosmos,
    /// the in-memory mock). Cloudflare KV lacks one — the counter races under concurrency,
    /// which suits a soft limit but not a hard quota.
    pub fn kv<S>(limit: u64, window: Duration) -> Self
    where
        S: Extractor + core::ops::Deref<Target = Kv>,
        S::Error: fmt::Debug,
    {
        Self::new(limit, window, Arc::new(kv_resolver::<S>))
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
            limit,
            window,
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
    ) -> impl Future<Output = Result<Response, Error>> + Send {
        async move {
            let store = match (self.resolve)(request).await {
                Ok(store) => store,
                // A missing binding is a permanent misconfiguration, not an outage —
                // log it even while failing open so the limiter never silently dies.
                Err(error) if self.fail_open => {
                    tracing::warn!(
                        "rate limit store could not resolve, admitting request: {error}"
                    );
                    return next.run(request).await;
                }
                Err(error) => {
                    tracing::warn!("rate limit store could not resolve: {error}");
                    return Err(Error::from(RateLimitUnavailable::new()));
                }
            };
            let key = format!("rate:{}:{}", self.name, (self.key)(request).await?);
            match store.hit(&key, self.window).await {
                Ok(hit) if hit.count <= self.limit => next.run(request).await,
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

/// Milliseconds since the Unix epoch, on every target.
fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as u64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A `Kv`-backed counter: fixed window indexed by epoch, so `Retry-After` is exact.
#[derive(Debug)]
struct KvStore {
    kv: Kv,
}

impl RateLimitStore for KvStore {
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit, HitError>> {
        Box::pin(async move {
            // Millisecond indexing, same as the in-memory store — a sub-second window must
            // not silently widen to a second on one backend and not the other.
            let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
            let now = now_ms();
            let index = now / window_ms;
            let bucket = format!("rl:{key}:{index}");
            let count = self
                .kv
                .increment(&bucket, 1)
                .await
                .map_err(|error| match error {
                    // No atomic increment — the Cloudflare case — is a capability gap, not a
                    // transient outage.
                    KvError::Unsupported(_) => HitError::Unsupported(Error::new(error)),
                    other => HitError::Backend(Error::new(other)),
                })?
                .max(0)
                .cast_unsigned();
            if count == 1 {
                // First hit of the window — set the TTL once. A crash between increment and
                // expire leaves a permanent counter; the window key still rolls forward.
                let _ = self.kv.expire(&bucket, window.saturating_mul(2)).await;
            }
            Ok(Hit {
                count,
                retry_after: ((index + 1) * window_ms).saturating_sub(now) / 1000,
            })
        })
    }
}

/// An in-process counter for single-binary deploys and tests.
#[derive(Debug, Default)]
struct MemoryStore {
    buckets: Mutex<HashMap<String, (u64, u64)>>,
}

/// The map sweep threshold — past it a `hit` first drops expired buckets so an unbounded
/// key space cannot grow memory without bound.
const MAX_MEMORY_BUCKETS: usize = 4096;

impl RateLimitStore for MemoryStore {
    #[allow(clippy::significant_drop_tightening)]
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit, HitError>> {
        Box::pin(std::future::ready({
            // `hit` can be called on a store directly, outside the layer's assertion —
            // treat a sub-millisecond window as 1ms rather than divide by zero.
            let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX).max(1);
            let now = now_ms();
            let index = now / window_ms;
            let bucket = format!("rl:{key}:{index}");
            let (count, reset) = {
                let mut buckets = self
                    .buckets
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if buckets.len() > MAX_MEMORY_BUCKETS {
                    buckets.retain(|_, (_, reset)| *reset > now);
                }
                if buckets.len() > MAX_MEMORY_BUCKETS {
                    // Still over cap — the key space outruns expiry (attacker-churned keys
                    // inside one live window). Drop the buckets closest to expiring until
                    // within bounds: bounding memory matters more than losing the oldest
                    // window a little early.
                    let overflow = buckets.len() - MAX_MEMORY_BUCKETS;
                    let mut resets: Vec<(String, u64)> = buckets
                        .iter()
                        .map(|(bucket, (_, reset))| (bucket.clone(), *reset))
                        .collect();
                    resets.sort_unstable_by_key(|(_, reset)| *reset);
                    for (bucket, _) in resets.into_iter().take(overflow) {
                        buckets.remove(&bucket);
                    }
                }
                let entry = buckets
                    .entry(bucket)
                    .or_insert((0, (index + 1) * window_ms));
                if now >= entry.1 {
                    *entry = (0, (index + 1) * window_ms);
                }
                entry.0 += 1;
                (entry.0, entry.1)
            };
            Ok(Hit {
                count,
                retry_after: (reset - now) / 1000,
            })
        }))
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
                _: Duration,
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
}
