//! A simple example.

use skyzen::routing::{CreateRouteNode, Params, Route, Router};
use skyzen::Result as SkyResult;

async fn health() -> &'static str {
    "OK"
}

async fn root() -> &'static str {
    "Hello from Skyzen!"
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
    ))
    .build()
}

#[cfg(target_arch = "wasm32")]
#[doc = "Worker entry point used when targeting Cloudflare Workers."]
#[skyzen::main]
async fn worker_entry() -> Router {
    build_router()
}

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
#[skyzen::main]
fn main() -> Router {
    build_router()
}
