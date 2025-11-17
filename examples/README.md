# Examples

The `examples` directory demonstrates how to target both native and wasm runtimes with the same Skyzen APIs. Run them from the repository root.

## `native.rs`

Features:

- Builds a `Router` with nested routes (`/hello`, `/hello/{name}`, `/healthz`).
- Uses the `Query` extractor and `Params` to parse query strings and path parameters.
- Returns strongly typed JSON via the `Json<T>` responder.

Run locally (defaults to `127.0.0.1:8787`, override with CLI flags such as `--port 3000`):

```sh
cargo run --example native -- --port 3000
```

Then visit `http://127.0.0.1:3000/hello?name=Skyzen&excited=true`.

## `worker.rs`

Features:

- Single `#[skyzen::main]` entry that compiles to both native binaries and WinterCG `fetch` handlers.
- Simple text routes you can interrogate via `curl` or Cloudflare Worker previews.

Run natively (helpful during development because you get logging, CLI overrides, and Ctrl+C handling automatically):

```sh
cargo run --example worker
```

Build for wasm32 targets and upload to Cloudflare Workers:

```sh
rustup target add wasm32-unknown-unknown
cargo build --example worker --target wasm32-unknown-unknown --release
wasm-bindgen --target web target/wasm32-unknown-unknown/release/examples/worker.wasm --out-dir worker-dist
```

Deploy the generated artifacts with `wrangler publish worker-dist/worker.js` (create `worker.js` that imports the wasm bundle and forwards `fetch` to it).

## `openapi.rs`

Features:

- Demonstrates `#[skyzen::openapi]` for handlers along with typed `Json<T>` extractors/responders.
- Shows how to build a `Router`, call `.openapi()`, and inspect the collected operations.
- Prints schemas and doc comments when compiled in debug mode (release builds disable OpenAPI instrumentation and log an explicit message).

Run it locally:

```sh
cargo run --example openapi
```
