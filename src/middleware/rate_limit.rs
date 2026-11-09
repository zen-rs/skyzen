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
    header::{HeaderValue, RETRY_AFTER},
    http_error, Request, Response, StatusCode,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor,
};
use skyzen_services::Kv;

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

/// The counter backend — `Kv` bindings and the in-process map both fill it.
#[allow(clippy::manual_async_fn)]
trait RateLimitStore: fmt::Debug + Send + Sync + 'static {
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit, Error>>;
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
            fail_open: true,
        }
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
                Err(_) if self.fail_open => return next.run(request).await,
                Err(_) => return Err(Error::from(RateLimitUnavailable::new())),
            };
            let key = (self.key)(request).await?;
            match store.hit(&key, self.window).await {
                Ok(hit) if hit.count <= self.limit => next.run(request).await,
                Ok(hit) => {
                    let mut response = skyzen_core::error_response(&RateLimited::new());
                    response
                        .headers_mut()
                        .insert(RETRY_AFTER, HeaderValue::from(hit.retry_after.max(1)));
                    Ok(response)
                }
                Err(_) if self.fail_open => next.run(request).await,
                Err(_) => Err(Error::from(RateLimitUnavailable::new())),
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
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit, Error>> {
        Box::pin(async move {
            let window_secs = window.as_secs().max(1);
            let now_secs = now_ms() / 1000;
            let index = now_secs / window_secs;
            let bucket = format!("rl:{key}:{index}");
            let count = self
                .kv
                .increment(&bucket, 1)
                .await
                .map_err(Error::new)?
                .max(0)
                .cast_unsigned();
            if count == 1 {
                // First hit of the window — set the TTL once. A crash between increment and
                // expire leaves a permanent counter; the window key still rolls forward.
                let _ = self.kv.expire(&bucket, window.saturating_mul(2)).await;
            }
            Ok(Hit {
                count,
                retry_after: (index + 1) * window_secs - now_secs,
            })
        })
    }
}

/// An in-process counter for single-binary deploys and tests.
#[derive(Debug, Default)]
struct MemoryStore {
    buckets: Mutex<HashMap<String, (u64, u64)>>,
}

impl RateLimitStore for MemoryStore {
    #[allow(clippy::significant_drop_tightening)]
    fn hit<'a>(&'a self, key: &'a str, window: Duration) -> BoxFuture<'a, Result<Hit, Error>> {
        Box::pin(std::future::ready({
            let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
            let now = now_ms();
            let index = now / window_ms;
            let bucket = format!("rl:{key}:{index}");
            let (count, reset) = {
                let mut buckets = self
                    .buckets
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    use http_kit::header::{HeaderValue, RETRY_AFTER};
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
    async fn requests_over_the_limit_answer_429_with_retry_after() {
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
        let response = router.go(get()).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = response.headers()[RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=60).contains(&retry_after));
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
        assert_eq!(
            router.go(a2).await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
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
            ) -> futures_core::future::BoxFuture<'a, Result<super::Hit, skyzen_core::Error>>
            {
                Box::pin(std::future::ready(Err(skyzen_core::Error::msg("kv down"))))
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
