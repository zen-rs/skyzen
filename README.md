# Skyzen

Skyzen is a router-first HTTP framework that targets both native servers (Tokio + Hyper) and modern edge platforms powered by WebAssembly. The project is layered as a Cargo workspace containing the main `skyzen` crate, shared traits in `skyzen-core`, macros in `skyzen-macros`, and an optional Hyper backend in `skyzen-hyper`.

## Highlights

- **Single entry point macro** – annotate any function that returns a `Router` or an `impl Endpoint + Clone + Sync` with `#[skyzen::main]` and Skyzen wires up both the native `main` function and the WinterCG-compatible `fetch` handler for wasm targets.
- **Native niceties out of the box** – pretty logging, `Ctrl+C` graceful shutdown, and CLI overrides for host/port (`--listen`, `--addr`, `--host`, `--port` or `-p`) are enabled automatically.
- **Wasm anywhere** – the wasm runtime works with the standard `fetch(Request, Env, Context)` signature which runs unmodified on Cloudflare Workers, Deno Deploy, Fastly Compute@Edge, and other WinterCG implementations.
- **Routing + extractors + responders** – compose routers via `Route::new`, extract strongly-typed data from requests, plug in middleware stacks, or respond with anything implementing `Responder`.
- **Shared core traits** – `skyzen-core` defines the server traits consumed by both native and wasm stacks, keeping abstractions decoupled from any specific runtime.

## Quick start

Add Skyzen to your project (from this workspace, use a path dependency; otherwise use the published crate):

```toml
[dependencies]
skyzen = "0.1"
```

Create a router and annotate your entry function:

```rust
use skyzen::{
    routing::{CreateRouteNode, Params, Route, Router},
    Result,
};

async fn health() -> &'static str {
    "OK"
}

async fn greet(params: Params) -> Result<String> {
    let name = params.get("name")?;
    Ok(format!("Hello, {name}!"))
}

fn router() -> Router {
    Route::new((
        "/".at(|| async { "Hello from Skyzen!" }),
        "/health".at(health),
        "/hello/{name}".at(greet),
    ))
    .build()
}

#[skyzen::main]
fn main() -> Router {
    router()
}
```

Running `cargo run` starts a development server on `127.0.0.1:8787`. Clients can now hit `GET /`, `GET /health`, or `GET /hello/alex`.

### Using any endpoint type

Instead of returning `Router`, you may return any type implementing `Endpoint + Clone + Sync + Send + 'static`. This enables advanced scenarios such as building middleware stacks, composing routers dynamically, or wiring other protocol servers.

## Native mode

When compiled for non-wasm targets the `#[skyzen::main]` macro generates a concrete `fn main()` that:

1. Installs `color-eyre` + `tracing_subscriber` to provide colorful logs and pretty error reports (respecting `RUST_LOG`).
2. Reads CLI overrides (`--listen`, `--addr`, `--host`, `--port`, `-p`) and writes the resolved address into the `SKYZEN_ADDRESS` environment variable (default `127.0.0.1:8787`).
3. Boots a multi-threaded Tokio runtime, spawns a Hyper server, and begins serving the router.
4. Listens for `Ctrl+C` and performs a graceful shutdown of the accept loop, logging final status.

Because Skyzen sits on top of the async `http-kit` stack, all handlers are async by default and can leverage the existing ecosystem (SQLx, reqwest, redis, etc.) with zero extra setup.

## Wasm mode

Compiling the same code for wasm32 (`cargo build --target wasm32-unknown-unknown`) changes the macro output so that no native `main` exists. Instead, the macro exports a `#[wasm_bindgen] pub async fn fetch(request, env, ctx)` that:

1. Lazily constructs your router/endpoint.
2. Converts the incoming `web_sys::Request` into a Skyzen `Request`.
3. Executes the endpoint and transforms the `Response` back into a WinterCG-compatible `web_sys::Response`.

That makes it straightforward to deploy to Cloudflare Workers or any environment that expects the `fetch` contract:

```sh
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release
wasm-bindgen --target web target/wasm32-unknown-unknown/release/your_app.wasm --out-dir worker
```

Upload the generated artifacts (plus your Worker bootstrap script) and Cloudflare will call the exported `fetch` just like any other Worker. Because the runtime does not rely on Tokio, no additional shims are necessary.

## Workspace layout

- `src/` – the primary `skyzen` crate containing routing, middleware, responders, runtime helpers, and extractors.
- `core/` – the `skyzen-core` crate that hosts foundational traits (`RequestHandler`, middleware traits, server abstractions) reusable by alternative runtimes.
- `hyper/` – the optional Hyper backend (`skyzen-hyper`) that integrates Hyper’s executor with `skyzen-core`.
- `skyzen-macros/` – proc-macros such as `#[skyzen::main]`.
- `examples/` – runnable examples for native + wasm targets; see `examples/README.md` for usage and build commands.

## Local development

From the repository root:

```sh
cargo fmt
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
```

`cargo fmt` ensures consistent style, `cargo clippy` enforces the workspace lint list (including pedantic/nursery groups), and `cargo test` validates every crate and feature combination. Use `RUST_LOG=debug` while debugging integration tests or routers that rely on verbose tracing.

## Testing on wasm

- Native tests: `cargo test --all-features`
- Wasm smoke test (headless): `cargo test -p skyzen --target wasm32-unknown-unknown --no-default-features` (requires `wasm-bindgen-test` runner)
- Cloudflare preview: `wrangler dev --local target/wasm32-unknown-unknown/release/your_app.wasm`

## License

Skyzen is available under the terms of the MIT or Apache-2.0 license, at your option.
