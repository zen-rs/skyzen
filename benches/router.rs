//! In-memory `Router::go` dispatch benchmarks: the per-request work the issue
//! <https://github.com/zen-rs/skyzen/issues/173> trims — route lookup, extension inserts,
//! dispatch future boxing and middleware chains — with no server or transport involved.
//!
//! Verify the benches still measure rather than break under `cargo test --bench router` (or
//! `cargo bench --bench router -- --test`), which runs each benchmark once instead of
//! collecting statistics.

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use futures_lite::future::block_on;
use skyzen::{
    extract::Path,
    middleware::{from_fn, Next},
    routing::{CreateRouteNode, Route},
    Body, Request, Result,
};

fn request(path: &str) -> Request {
    let mut request = Request::new(Body::empty());
    *request.uri_mut() = path.parse().expect("valid path");
    request
}

// `{name}`/`{id}` are route captures, not format args.
#[allow(clippy::literal_string_with_formatting_args)]
fn bench_dispatch(c: &mut Criterion) {
    let static_router = Route::new(("/ping".at(|| async { Result::Ok("pong") }),)).build();

    let params_router = Route::new(("/users/{name}/posts/{id}".at(
        |Path((name, id)): Path<(String, u32)>| async move { Result::Ok(format!("{name}#{id}")) },
    ),))
    .build();

    let passthrough = || {
        from_fn(|request: &mut Request, next: Next<'_>| {
            Box::pin(async move { next.run(request).await })
        })
    };
    let layered_router = Route::new(("/work".at(|| async { Result::Ok("done") }),))
        .with(passthrough())
        .layer(passthrough())
        .layer(passthrough())
        .layer(passthrough())
        .build();

    let mut group = c.benchmark_group("router");
    for (name, router, path) in [
        ("static", &static_router, "/ping"),
        ("params", &params_router, "/users/lexo/posts/42"),
        ("middleware", &layered_router, "/work"),
        ("not_found", &static_router, "/missing"),
    ] {
        group.bench_with_input(BenchmarkId::new("dispatch", name), path, |b, path| {
            b.iter_batched(
                || request(path),
                |request| block_on(router.clone().go(request)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
