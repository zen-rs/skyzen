//! Edge example demonstrating how the `#[skyzen::main]` macro maps to
//! Cloudflare Workers (or any `WinterCG` runtime) without extra glue.

use skyzen::routing::{CreateRouteNode, Params, Route, Router};
#[cfg(target_arch = "wasm32")]
use skyzen::runtime::CfProperties;
use skyzen::runtime::WorkerContext;
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

/// Hand work to the runtime that outlives the response.
///
/// The same call is correct on both targets, which is the point: on Workers the isolate is kept
/// alive until the future finishes, and on the built-in native runtime the task is spawned *and*
/// joined by graceful shutdown. Without the context, a future spawned here would run natively and
/// be silently cancelled on Workers.
async fn accept_work(context: WorkerContext) -> SkyResult<&'static str> {
    context.wait_until(async {
        tracing::info!("post-response work finished after the response was returned");
    })?;
    Ok("accepted")
}

/// Report the edge metadata Cloudflare attached to the request.
#[cfg(target_arch = "wasm32")]
async fn where_am_i(cf: CfProperties) -> String {
    format!(
        "colo={} country={} tls={}",
        cf.colo.as_deref().unwrap_or("unknown"),
        cf.country.as_deref().unwrap_or("unknown"),
        cf.tls_version.as_deref().unwrap_or("unknown"),
    )
}

/// `CfProperties` does not exist off `wasm32`, so this build has nothing to report — the type is
/// deliberately absent rather than returning empty values that would read as a real answer.
#[cfg(not(target_arch = "wasm32"))]
async fn where_am_i() -> &'static str {
    "request.cf is a Cloudflare value and this build is not running on Workers"
}

fn build_router() -> Router {
    Route::new((
        "/".at(root),
        "/health".at(health),
        "/hello".route(("/{name}".at(greet),)),
        "/readyz".at(|| async { "ready" }),
        "/track".at(accept_work),
        "/where-am-i".at(where_am_i),
    ))
    .build()
}

#[skyzen::main]
fn worker() -> Router {
    build_router()
}

// Example-only shim: Cargo examples are binaries.
// Real serverless apps should use a normal lib crate (`cdylib`) and don't need this.
#[cfg(target_arch = "wasm32")]
fn main() {}
