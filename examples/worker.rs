//! Edge example demonstrating how the `#[skyzen::main]` macro maps to
//! Cloudflare Workers (or any `WinterCG` runtime) without extra glue.

use skyzen::routing::{CreateRouteNode, Params, Route, Router};
use skyzen::Result as SkyResult;

async fn health() -> &'static str {
    "OK"
}

async fn root() -> &'static str {
    "Hello from Skyzen running at the edge!"
}

async fn greet(params: Params) -> SkyResult<String> {
    let name = params.get("name")?;
    Ok(format!("Hello, {name}!"))
}

fn build_router() -> Router {
    Route::new((
        "/".at(root),
        "/health".at(health),
        "/hello".route(("/{name}".at(greet),)),
        "/readyz".at(|| async { "ready" }),
    ))
    .build()
}

#[skyzen::main]
fn worker() -> Router {
    build_router()
}
