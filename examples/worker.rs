//! Edge example demonstrating how the `#[skyzen::main]` macro maps to
//! Cloudflare Workers (or any `WinterCG` runtime) without extra glue.

use std::sync::atomic::{AtomicU64, Ordering};

use skyzen::durable::DurableObject;
use skyzen::extract::Path;
use skyzen::routing::{CreateRouteNode, Route, Router};
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

async fn greet(Path(name): Path<String>) -> String {
    format!("Hello, {name}!")
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

/// A Durable Object that counts the events its instance has seen.
///
/// The count lives in memory — `PERSIST = false`, nothing is written — so it climbing across
/// requests is the proof that the platform keeps one object per instance and delivers every event
/// to it, rather than rebuilding the struct around each one.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[skyzen::durable_object]
pub struct Visits {
    hits: AtomicU64,
}

impl DurableObject for Visits {
    const PERSIST: bool = false;

    fn fetch(&self) -> Router {
        let hits = self.hits.fetch_add(1, Ordering::Relaxed) + 1;
        Route::new(("/hits".at(move || async move { hits.to_string() }),)).build()
    }
}

/// A Durable Object failure is a server fault to the caller of this route.
fn durable(error: skyzen::durable::DurableObjectError) -> skyzen::Error {
    skyzen::Error::new(error)
}

/// Count a visit to `name`'s object and relay how many it has seen.
#[cfg(target_arch = "wasm32")]
async fn visit(
    Path(name): Path<String>,
    env: skyzen::runtime::wasm::WasmEnv,
) -> SkyResult<skyzen::Response> {
    let visits =
        skyzen_cloudflare::CfDurableNamespace::from_env(env.as_js(), "VISITS").map_err(durable)?;
    visits
        .get_by_name(&name)
        .map_err(durable)?
        .fetch_url("https://visits/hits")
        .await
        .map_err(durable)
}

/// The same route against the in-process simulator, which keeps one object per id the way the
/// platform does.
#[cfg(not(target_arch = "wasm32"))]
async fn visit(
    Path(name): Path<String>,
    skyzen::utils::State(visits): skyzen::utils::State<
        skyzen::durable::NativeDurableNamespace<Visits>,
    >,
) -> SkyResult<skyzen::Response> {
    visits
        .get_by_name(&name)
        .map_err(durable)?
        .fetch_url("https://visits/hits")
        .await
        .map_err(durable)
}

fn build_router() -> Router {
    let routes = Route::new((
        "/".at(root),
        "/health".at(health),
        "/hello".route(("/{name}".at(greet),)),
        "/readyz".at(|| async { "ready" }),
        "/track".at(accept_work),
        "/where-am-i".at(where_am_i),
        "/visits".route(("/{name}".at(visit),)),
    ));
    // On Workers the namespace is a binding the handler reads from the environment; natively
    // the simulator is the binding, and it is handed to the handler as state.
    #[cfg(not(target_arch = "wasm32"))]
    let routes = routes.with(skyzen::utils::State(
        skyzen::durable::NativeDurableNamespace::<Visits>::new(),
    ));
    routes.build()
}

#[skyzen::main]
fn worker() -> Router {
    build_router()
}

// Example-only shim: Cargo examples are binaries.
// Real serverless apps should use a normal lib crate (`cdylib`) and don't need this.
#[cfg(target_arch = "wasm32")]
fn main() {}
