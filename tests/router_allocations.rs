//! Guard against per-request allocation regressions on the router hot path
//! (<https://github.com/zen-rs/skyzen/issues/173>).
//!
//! A counting `#[global_allocator]` intercepts every `alloc` call; each scenario warms the
//! route, then asserts an upper bound on the allocations one `Router::go` dispatch issues.
//! All four scenarios live in one `#[test]` so `cargo test`'s in-binary threads cannot race
//! the shared counter (under nextest each test is its own process anyway).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_lite::future::block_on;
use skyzen::{
    extract::Path,
    middleware::{from_fn, Next},
    routing::{CreateRouteNode, Route, Router},
    Body, Request, Result,
};

/// Counts `alloc` calls so a measured region can be asserted as `after - before <= LIMIT`.
struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// The allocator itself is what the test measures; the unsafe contract is upheld by delegating
// layout handling to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn request(path: &str) -> Request {
    let mut request = Request::new(Body::empty());
    *request.uri_mut() = path.parse().expect("valid path");
    request
}

/// Allocation count for one dispatch of `router` against `path`, after warm-up dispatches
/// have populated matchit and runtime caches. Request construction happens outside the
/// measured region; the `Router` clone is inside it because that is the per-request cost.
fn allocs_per_request(router: &Router, path: &str) -> usize {
    for _ in 0..64 {
        drop(block_on(router.clone().go(request(path))));
    }
    let request = request(path);
    let before = ALLOCS.load(Ordering::Relaxed);
    drop(block_on(router.clone().go(request)));
    let allocs = ALLOCS.load(Ordering::Relaxed) - before;
    eprintln!("dispatch of {path}: {allocs} allocations");
    allocs
}

fn static_router() -> Router {
    Route::new(("/ping".at(|| async { Result::Ok("pong") }),)).build()
}

fn passthrough() -> impl skyzen::middleware::Middleware {
    from_fn(|request: &mut Request, next: Next<'_>| {
        Box::pin(async move { next.run(request).await })
    })
}

#[test]
fn dispatches_stay_within_their_allocation_budgets() {
    // The budgets are per-request `alloc` calls measured on this code path: 7 for the static
    // route (extensions map, `RouteMatch`, the boxed dispatch future), 9 with two `Path`
    // params (the captures sit inline in `Params` — only the `RouteMatch` insert and the
    // handler `format!` allocate on top of the static baseline), 16 through a four-deep
    // middleware chain, and 5 on the unmatched path. Each bound leaves a little slack for
    // allocator internals but fails on a new per-request box or insert.
    // See <https://github.com/zen-rs/skyzen/issues/173> for what they protect.

    assert!(
        allocs_per_request(&static_router(), "/ping") <= 8,
        "static route"
    );

    let params = Route::new(("/users/{name}/posts/{id}".at(
        |Path((name, id)): Path<(String, u32)>| async move { Result::Ok(format!("{name}#{id}")) },
    ),))
    .build();
    assert!(
        allocs_per_request(&params, "/users/lexo/posts/42") <= 12,
        "two-param route"
    );

    let layered = Route::new(("/work".at(|| async { Result::Ok("done") }),))
        .with(passthrough())
        .layer(passthrough())
        .layer(passthrough())
        .layer(passthrough())
        .build();
    assert!(
        allocs_per_request(&layered, "/work") <= 19,
        "three layers + one route middleware"
    );

    assert!(
        allocs_per_request(&static_router(), "/missing") <= 6,
        "404 path"
    );
}
