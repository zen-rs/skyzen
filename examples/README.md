# Examples

The `examples` directory demonstrates how to target both native and WASM runtimes with the same Skyzen APIs. Run them from the repository root.

Most are single-file `--example` targets of the root crate. `queue-consumer/` is a crate of its own, because the macros read the `Skyzen.toml` sitting next to the crate they compile: a manifest for a single-file example would be the whole framework's manifest.

## `native.rs`

Features:

- Builds a `Router` with nested routes (`/hello`, `/hello/{name}`, `/healthz`).
- Uses the `Query<T>` and `Path<T>` extractors, so neither the query string nor the path segment is parsed by hand.
- Returns strongly typed JSON via the `Json<T>` responder.

Run locally. Skyzen binds an available localhost port by default and logs it, or you can pin one with CLI flags such as `--port 3000`:

```sh
cargo run --example native -- --port 3000
```

Then visit `http://127.0.0.1:3000/hello?name=Skyzen&excited=true`.

## `worker.rs`

Features:

- Single `#[skyzen::main]` entry that compiles to both native binaries and WinterCG `fetch` handlers.
- Simple text routes you can interrogate via `curl` or Cloudflare Worker previews.
- `WorkerContext::wait_until` for work that outlives the response — correct on both targets, which is the point.
- `CfProperties` for Cloudflare's `request.cf` edge metadata, `#[cfg]`-gated because the type genuinely does not exist off `wasm32`.

Note: this file is an `example` target, so Cargo treats it as a binary.
For real serverless deployment, prefer a normal `lib` crate with:

```toml
[lib]
crate-type = ["cdylib", "rlib"]
```

Run natively (helpful during development because you get logging, CLI overrides, and Ctrl+C handling automatically):

```sh
cargo run --example worker
```

For real Cloudflare local simulation or deployment, prefer a normal project with `Skyzen.toml`.
Skyzen CLI will build the wasm target, generate bindings, write the Worker shim, and then call Wrangler for you:

```sh
skyzen dev --provider cloudflare
skyzen deploy --provider cloudflare
```

## `openapi.rs`

Features:

- Demonstrates `#[skyzen::openapi]` for handlers along with typed `Json<T>` extractors/responders.
- Shows how to build a `Router`, call `.openapi()`, and inspect the collected operations.
- Prints schemas and doc comments from the generated `OpenAPI` description.

Run it locally:

```sh
cargo run --example openapi
```

## `openapi_full.rs`

Features:

- Extended OpenAPI example with multiple annotated handlers.
- Demonstrates `Route::openapi()` and `OpenApi::redoc_route("/docs")` to serve ReDoc documentation.
- Shows typed request/response schemas with `#[skyzen::openapi]`, and `State<T>` shared across handlers.

```sh
cargo run --example openapi_full
```

## `websocket_echo.rs`

Features:

- Unified WebSocket API that works on both native and WASM targets.
- Demonstrates text, JSON (`recv_json`/`send`), and binary message handling.
- Uses the `.ws()` route convenience method.

```sh
cargo run --example websocket_echo
```

## `embed_hyper.rs`

Features:

- Demonstrates embedding Skyzen into a custom runtime without the `#[skyzen::main]` macro.
- Uses `smol` as the async runtime instead of Tokio.
- Shows direct use of `Hyper.serve()` with a custom executor and TCP listener.

Requires the `hyper` feature:

```sh
cargo run --example embed_hyper
```

## `services.rs`

Features:

- Demonstrates portable service abstractions with `Kv` and `Storage` as handler extractors.
- Uses `InMemoryKv` and `InMemoryStorage` for local development.
- Shows how to swap to Redis/S3 with a one-line change (commented examples in code).

```sh
cargo run --example services
```

## `queue-consumer/`

Features:

- Runs a `#[skyzen::queue]` handler natively: `[[native.queue_consumer]]` in `Skyzen.toml` makes Skyzen poll the queue, invoke the handler and settle each batch, beside the HTTP server.
- The same handler a Cloudflare Worker would run for pushed batches, unchanged.
- Shows the enqueue side and the consume side sharing one queue instance, and a retry picking up the manifest's `retry_delay`.

```sh
cargo run -p skyzen-example-queue-consumer -- --port 3000
curl -X POST localhost:3000/jobs -H 'content-type: application/json' -d '{"id":"1","action":"ship"}'
curl -X POST localhost:3000/jobs -H 'content-type: application/json' -d '{"id":"2","action":"retry"}'
```
