//! `ConcurrencyLimit` — cap in-flight requests with load shedding or bounded queueing.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    mem,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use futures_core::Stream;
use futures_util::future::{select, Either};
use http_kit::{
    http_error, utils::Bytes, Body, BodyError, DataStream, Request, Response, StatusCode,
};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error,
};

use super::timeout::delay;

http_error!(
    /// `503 Service Unavailable` — the server is at its concurrency capacity.
    pub Overloaded,
    StatusCode::SERVICE_UNAVAILABLE,
    "server is at concurrency capacity"
);

#[derive(Debug, Clone, Copy)]
enum Mode {
    Shed,
    Queue(Duration),
}

/// Cap the number of in-flight requests handled concurrently.
///
/// By default, requests arriving when `max_in_flight` slots are already occupied are shed
/// immediately with `503 Service Unavailable` ([`Overloaded`]). Call [`ConcurrencyLimit::queue`]
/// to instead wait up to a bounded duration for a slot to open.
///
/// For streaming responses (`body.len().is_none()`), the concurrency permit is held until the
/// response body stream finishes or is dropped, so slow readers count toward the limit.
#[derive(Debug, Clone)]
pub struct ConcurrencyLimit {
    state: Arc<SemaphoreState>,
    mode: Mode,
}

impl ConcurrencyLimit {
    /// Create a concurrency limiter allowing at most `max_in_flight` concurrent requests,
    /// shedding excess load immediately with `503 Service Unavailable`.
    ///
    /// # Panics
    ///
    /// Panics if `max_in_flight == 0`.
    #[must_use]
    pub fn new(max_in_flight: usize) -> Self {
        assert!(max_in_flight > 0, "max_in_flight must be at least 1");
        Self {
            state: Arc::new(SemaphoreState {
                inner: Mutex::new(SemaphoreInner {
                    active: 0,
                    max: max_in_flight,
                    waiters: VecDeque::new(),
                }),
            }),
            mode: Mode::Shed,
        }
    }

    /// Shed excess requests immediately with `503 Service Unavailable` (the default).
    #[must_use]
    pub const fn shed(mut self) -> Self {
        self.mode = Mode::Shed;
        self
    }

    /// Wait up to `max_wait` in FIFO order for a concurrency slot before shedding with `503`.
    #[must_use]
    pub const fn queue(mut self, max_wait: Duration) -> Self {
        self.mode = Mode::Queue(max_wait);
        self
    }

    /// Number of requests currently holding a concurrency slot.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
    }

    async fn acquire(&self) -> Result<Permit, Overloaded> {
        match self.try_acquire() {
            Ok(permit) => Ok(permit),
            Err(None) => Err(Overloaded::new()),
            Err(Some(waiter)) => {
                let Mode::Queue(max_wait) = self.mode else {
                    unreachable!("waiter only created in Queue mode");
                };
                let wait_fut = std::pin::pin!(WaitForPermit {
                    state: Arc::clone(&self.state),
                    waiter,
                    acquired: false,
                });
                let timeout_fut = std::pin::pin!(delay(max_wait));
                match select(wait_fut, timeout_fut).await {
                    Either::Left((permit, _)) => Ok(permit),
                    Either::Right(((), _)) => Err(Overloaded::new()),
                }
            }
        }
    }

    fn try_acquire(&self) -> Result<Permit, Option<Arc<Mutex<Waiter>>>> {
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.active < inner.max {
            inner.active += 1;
            return Ok(Permit {
                state: Arc::clone(&self.state),
            });
        }
        match self.mode {
            Mode::Shed => Err(None),
            Mode::Queue(_) => {
                let waiter = Arc::new(Mutex::new(Waiter {
                    ready: false,
                    cancelled: false,
                    waker: None,
                }));
                inner.waiters.push_back(Arc::clone(&waiter));
                drop(inner);
                Err(Some(waiter))
            }
        }
    }
}

impl Middleware for ConcurrencyLimit {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let permit = self.acquire().await.map_err(Error::from)?;
        let mut response = next.run(request).await?;
        if response.body().len().is_none() {
            let body = mem::take(response.body_mut());
            *response.body_mut() = Body::from_stream(PermitStream {
                stream: body.into_data_stream(),
                _permit: permit,
            });
        }
        Ok(response)
    }
}

struct SemaphoreState {
    inner: Mutex<SemaphoreInner>,
}

impl fmt::Debug for SemaphoreState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemaphoreState").finish_non_exhaustive()
    }
}

struct SemaphoreInner {
    active: usize,
    max: usize,
    waiters: VecDeque<Arc<Mutex<Waiter>>>,
}

struct Waiter {
    ready: bool,
    cancelled: bool,
    waker: Option<Waker>,
}

struct Permit {
    state: Arc<SemaphoreState>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        // Transfer the slot to the next non-cancelled waiter if one exists; otherwise decrement.
        let waker_to_wake = {
            let mut inner = self
                .state
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut transferred = false;
            let mut waker = None;
            while let Some(waiter_arc) = inner.waiters.pop_front() {
                let mut waiter = waiter_arc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if waiter.cancelled {
                    continue;
                }
                waiter.ready = true;
                waker = waiter.waker.take();
                drop(waiter);
                transferred = true;
                break;
            }
            if !transferred {
                inner.active = inner.active.saturating_sub(1);
            }
            waker
        };
        if let Some(waker) = waker_to_wake {
            waker.wake();
        }
    }
}

struct WaitForPermit {
    state: Arc<SemaphoreState>,
    waiter: Arc<Mutex<Waiter>>,
    acquired: bool,
}

impl Future for WaitForPermit {
    type Output = Permit;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ready = {
            let mut waiter = self
                .waiter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if waiter.ready {
                true
            } else {
                waiter.waker = Some(cx.waker().clone());
                false
            }
        };
        if ready {
            self.acquired = true;
            Poll::Ready(Permit {
                state: Arc::clone(&self.state),
            })
        } else {
            Poll::Pending
        }
    }
}

impl Drop for WaitForPermit {
    fn drop(&mut self) {
        if self.acquired {
            return;
        }
        let was_ready = {
            let mut waiter = self
                .waiter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if waiter.ready {
                waiter.ready = false;
                true
            } else {
                waiter.cancelled = true;
                waiter.waker = None;
                false
            }
        };
        if was_ready {
            // A permit was transferred to this waiter right before it was dropped;
            // construct and immediately drop a Permit so it passes to the next waiter
            // or decrements `active`.
            drop(Permit {
                state: Arc::clone(&self.state),
            });
        }
    }
}

struct PermitStream {
    stream: DataStream,
    _permit: Permit,
}

impl Stream for PermitStream {
    type Item = Result<Bytes, BodyError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::ConcurrencyLimit;
    use crate::{
        middleware::apply,
        routing::{CreateRouteNode, Route},
        Body, Endpoint, Request, Response, Result, StatusCode,
    };
    use futures_util::future::join;
    use std::{convert::Infallible, future::Future, time::Duration};

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().unwrap();
        request
    }

    #[tokio::test]
    async fn shed_mode_rejects_excess_with_503_and_releases_on_drop() {
        let limit = ConcurrencyLimit::new(1);
        let (tx, rx) = async_channel::bounded::<()>(1);

        let router = Route::new(("/slow".at(move || {
            let rx = rx.clone();
            async move {
                let _ = rx.recv().await;
                Result::Ok("ok")
            }
        }),))
        .layer(limit.clone())
        .build();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let req1 = get("/slow");
                let fut1 = router.clone();
                let h1 = tokio::task::spawn_local(async move { fut1.go(req1).await });

                // Give task 1 a moment to enter the handler and hold the permit.
                while limit.in_flight() == 0 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(limit.in_flight(), 1);

                let err = router.clone().go(get("/slow")).await.unwrap_err();
                assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);

                tx.send(()).await.unwrap();
                let resp = h1.await.unwrap().unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                assert_eq!(limit.in_flight(), 0);

                // Next request succeeds once permit is released.
                tx.send(()).await.unwrap();
                let resp2 = router.go(get("/slow")).await.unwrap();
                assert_eq!(resp2.status(), StatusCode::OK);
            })
            .await;
    }

    #[tokio::test]
    async fn queue_mode_waits_for_slot_and_times_out_when_expired() {
        let limit = ConcurrencyLimit::new(1).queue(Duration::from_millis(250));
        let (tx, rx) = async_channel::bounded::<()>(2);

        let router = Route::new(("/work".at(move || {
            let rx = rx.clone();
            async move {
                let _ = rx.recv().await;
                Result::Ok("done")
            }
        }),))
        .layer(limit.clone())
        .build();

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let r1 = router.clone();
                let h1 = tokio::task::spawn_local(async move { r1.go(get("/work")).await });
                while limit.in_flight() == 0 {
                    tokio::task::yield_now().await;
                }

                let r2 = router.clone();
                let h2 = tokio::task::spawn_local(async move { r2.go(get("/work")).await });
                async_io::Timer::after(Duration::from_millis(20)).await;

                // Release first, then second.
                tx.send(()).await.unwrap();
                tx.send(()).await.unwrap();

                let (res1, res2) = join(h1, h2).await;
                assert_eq!(res1.unwrap().unwrap().status(), StatusCode::OK);
                assert_eq!(res2.unwrap().unwrap().status(), StatusCode::OK);
                assert_eq!(limit.in_flight(), 0);

                // Now test queue timeout.
                let short_limit = ConcurrencyLimit::new(1).queue(Duration::from_millis(25));
                let (tx2, rx2) = async_channel::bounded::<()>(1);
                let short_router = Route::new(("/work".at(move || {
                    let rx = rx2.clone();
                    async move {
                        let _ = rx.recv().await;
                        Result::Ok("done")
                    }
                }),))
                .layer(short_limit.clone())
                .build();

                let sr1 = short_router.clone();
                let sh1 = tokio::task::spawn_local(async move { sr1.go(get("/work")).await });
                while short_limit.in_flight() == 0 {
                    tokio::task::yield_now().await;
                }

                let err = short_router.go(get("/work")).await.unwrap_err();
                assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);

                tx2.send(()).await.unwrap();
                sh1.await.unwrap().unwrap();
                assert_eq!(short_limit.in_flight(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn streaming_response_holds_permit_until_body_finishes() {
        #[derive(Clone)]
        struct StreamEndpoint;
        impl Endpoint for StreamEndpoint {
            type Error = Infallible;
            fn respond(
                &mut self,
                _request: &mut Request,
            ) -> impl Future<Output = std::result::Result<Response, Self::Error>> {
                let stream =
                    futures_util::stream::iter(vec![Ok::<_, Infallible>("chunk1"), Ok("chunk2")]);
                std::future::ready(Ok(Response::new(Body::from_stream(stream))))
            }
        }

        let limit = ConcurrencyLimit::new(1);
        let mut req = get("/");
        let resp = apply(&limit, &mut req, StreamEndpoint).await.unwrap();
        assert_eq!(limit.in_flight(), 1);
        let body = resp.into_body().into_string().await.unwrap();
        assert_eq!(body, "chunk1chunk2");
        assert_eq!(limit.in_flight(), 0);
    }
}
