//! Edge example demonstrating how the `#[skyzen::main]` macro maps to
//! Cloudflare Workers (or any `WinterCG` runtime) without extra glue.

use std::sync::atomic::{AtomicU64, Ordering};

use skyzen::durable::DurableObject;
use skyzen::extract::Path;
use skyzen::routing::{CreateRouteNode, Route, Router};
#[cfg(target_arch = "wasm32")]
use skyzen::runtime::CfProperties;
use skyzen::runtime::WorkerContext;
use skyzen::utils::Json;
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

/// The greeting `/gzipped` serves pre-encoded; the CI lane greps for it after one decode.
const GREETING: &str = "Hello from the encoded side!";

/// Answer a body that is already the encoded representation: gzip bytes plus the
/// `Content-Encoding` header naming them. The runtime has to serve those bytes untouched — a
/// client decoding once gets the greeting back, not a second gzip stream.
async fn gzipped() -> SkyResult<skyzen::Response> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(GREETING.as_bytes())
        .map_err(skyzen::Error::new)?;
    let body = encoder.finish().map_err(skyzen::Error::new)?;
    let mut response = skyzen::Response::new(skyzen::Body::from_bytes(body));
    response.headers_mut().insert(
        skyzen::header::CONTENT_ENCODING,
        skyzen::header::HeaderValue::from_static("gzip"),
    );
    Ok(response)
}

/// A plain JSON answer carrying no `Content-Encoding` of its own, so the platform stays free to
/// compress it for the wire — the case the manual-encode rule must leave alone.
async fn plain_json() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "encoding": "platform" }))
}

/// The bytes `/passthrough/body` answers with, which only ever exist as the upstream's JS stream.
#[cfg(target_arch = "wasm32")]
const PASSTHROUGH_BODY: &str = "Hello through the passthrough!";

/// A JS error out of the conversion is a server fault to the caller of these routes. Takes
/// ownership to match `Result<_, JsValue>::map_err`.
#[cfg(target_arch = "wasm32")]
#[allow(clippy::needless_pass_by_value)]
fn js_error(error: wasm_bindgen::JsValue) -> skyzen::Error {
    skyzen::Error::msg(format!("{error:?}"))
}

/// A response standing in for a fetched upstream: its body is a JS stream this worker never
/// reads, under a status and header the client must still see.
#[cfg(target_arch = "wasm32")]
fn upstream_response() -> SkyResult<web_sys::Response> {
    let headers = web_sys::Headers::new().map_err(js_error)?;
    headers.append("x-upstream", "kept").map_err(js_error)?;
    let init = web_sys::ResponseInit::new();
    init.set_status(203);
    init.set_headers_headers(&headers);
    web_sys::Response::new_with_opt_str_and_init(Some(PASSTHROUGH_BODY), &init).map_err(js_error)
}

/// Proxy the upstream with its body handed to the runtime as the same JS stream, never copied
/// through wasm; the CI lane reads the bytes, status and header on the client side.
#[cfg(target_arch = "wasm32")]
async fn passthrough_body() -> SkyResult<skyzen::Response> {
    skyzen::runtime::wasm::passthrough_js_response(&upstream_response()?).map_err(js_error)
}

/// Check the conversion where only the runtime can: the rendered JS response carries the very
/// stream object the upstream did, and a passthrough whose handler also wrote a body is refused.
#[cfg(target_arch = "wasm32")]
async fn passthrough_conversion() -> SkyResult<&'static str> {
    use skyzen::runtime::wasm::{into_js_response, passthrough_js_response};

    let upstream = upstream_response()?;
    let stream = upstream
        .body()
        .ok_or_else(|| skyzen::Error::msg("the upstream response has no body stream"))?;
    let rendered = into_js_response(passthrough_js_response(&upstream).map_err(js_error)?)
        .map_err(js_error)?;
    let rendered_stream = rendered
        .body()
        .ok_or_else(|| skyzen::Error::msg("the rendered response has no body stream"))?;
    if !js_sys::Object::is(stream.as_ref(), rendered_stream.as_ref()) {
        return Err(skyzen::Error::msg(
            "the rendered response carries a different stream than the upstream",
        ));
    }
    let header = rendered.headers().get("x-upstream").map_err(js_error)?;
    if rendered.status() != 203 || header.as_deref() != Some("kept") {
        return Err(skyzen::Error::msg(
            "the rendered response lost the upstream status or header",
        ));
    }

    let mut written = passthrough_js_response(&upstream_response()?).map_err(js_error)?;
    *written.body_mut() = skyzen::Body::from_bytes(PASSTHROUGH_BODY);
    if into_js_response(written).is_ok() {
        return Err(skyzen::Error::msg(
            "a passthrough with a written body was rendered",
        ));
    }
    Ok("same stream; written body refused")
}

/// There is no JS response to pass through off `wasm32`; both passthrough routes say so rather
/// than answering something that would read as the conversion having run.
#[cfg(not(target_arch = "wasm32"))]
async fn passthrough_body() -> &'static str {
    "a JS body passthrough exists only on Workers and this build is not running there"
}

#[cfg(not(target_arch = "wasm32"))]
use passthrough_body as passthrough_conversion;

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
        "/gzipped".at(gzipped),
        "/json".at(plain_json),
        "/passthrough".route((
            "/body".at(passthrough_body),
            "/conversion".at(passthrough_conversion),
        )),
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
