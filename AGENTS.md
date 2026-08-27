# AGENTS.md

This file provides guidance for AI agents and coding assistants working on the Skyzen codebase.

## Build and Development Commands

```sh
# Format check / fix
cargo fmt

# Workspace tests
cargo test --workspace --all-features

# Run a specific test
cargo test --workspace --all-features test_name

# Lint across all targets and features
cargo clippy --workspace --all-targets --all-features

# Build Cloudflare Workers crate (wasm32-only, excluded from default members)
cargo check -p skyzen-cloudflare --target wasm32-unknown-unknown
cargo clippy -p skyzen-cloudflare --target wasm32-unknown-unknown

# Build for wasm32 targets
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release

# Run CLI unit and scaffold integration tests
cargo test -p skyzen-cli

# Run runnable examples
cargo run --example native -- --port 3000
cargo run --example services
cargo run --example websocket_echo
cargo run --example openapi
cargo run --example embed_hyper
```

## Architecture Overview

Skyzen is a router-first HTTP framework targeting both native servers (Tokio + Hyper) and WebAssembly edge platforms (such as Cloudflare Workers). Handlers and services are written against platform-agnostic traits and types, allowing the exact same application code to run natively or compile to edge WebAssembly.

### Crate Structure

**Framework Core:**
- **`skyzen`** (`/`) - Main framework crate: routing, extractors, responders, middleware, static files, and runtime helpers.
- **`skyzen-core`** (`core/`) - Foundational traits (`Extractor`, `Responder`, `Server`) reusable by alternative runtimes. Supports `no_std` when `std` feature is disabled.
- **`skyzen-macros`** (`macros/`) - Procedural macros: `#[skyzen::main]`, `#[skyzen::error]`, `#[skyzen::openapi]`, `#[skyzen::queue]`, `#[skyzen::scheduled]`, `#[skyzen::email]`, `#[skyzen::tail]`, `#[skyzen::durable_object]`, `#[skyzen::test]`, `#[derive(HttpError)]`, plus the function-like `import_config!` and `embed_migrations!`.
- **`skyzen-manifest`** (`manifest/`) - The one typed `Skyzen.toml` schema, consumed by **both** `skyzen-macros` (at compile time) and `skyzen-cli` (at deploy time), so a section can never be accepted by one and rejected by the other. Every struct carries `deny_unknown_fields` and the `type`/`backend` discriminants are enums, so a typo or an unsupported value fails at parse time.
- **`skyzen-hyper`** (`hyper/`) - Hyper server adapter implementing the `Server` trait from `skyzen-core`.
- **`skyzen-lambda`** (`lambda/`) - AWS Lambda adapter: HTTP invocations and SQS batches, driven by its own Tokio runtime. Reached through the root crate's optional `lambda` feature, never named by an application.

**Services Abstraction & Testing:**
- **`skyzen-services`** (`services/`) - Platform-agnostic service traits (`KeyValueStore`, `ObjectStorage`, `MessageQueue`, `DbBackend`, `DbTransactionBackend`), type-erased extractors (`Kv`, `Storage`, `Queue`, `Db`, `DurableDb`, `DurableKv`), the portable migration runner (`migrate`), and the queue envelope codec (`queue::envelope`) that two independent crates share.
- **`skyzen-test`** (`test/`) - In-memory mock service implementations (`InMemoryKv`, `InMemoryStorage`, `InMemoryQueue`, `InMemoryDb`), HTTP `TestClient`, `TestContext` with a slot per capability (durable ones included), assertion helpers, and snapshot testing support via `insta`.

**Platform Implementations:**
- **`skyzen-redis`** (`redis/`) - Redis implementation of `KeyValueStore`, atomics included.
- **`skyzen-s3`** (`s3/`) - S3-compatible implementation of `ObjectStorage`, with streaming, ranges, multipart and presigning.
- **`skyzen-cloudflare`** (`cloudflare/`) - Cloudflare Workers implementations for KV, R2, Queues, D1 SQL, Durable Objects, the secrets store, `WorkerContext`/`CfProperties`, and the email/tail events (**wasm32-only**).
- **`skyzen-cloudflare-admin`** (`cloudflare-admin/`) - Cloudflare REST API client, used by `skyzen provision`.
- **`skyzen-aws`** (`aws/`) - AWS implementations: DynamoDB (`DynamoKv`), SQS (`SqsQueue`, FIFO and consume side), the Aurora Data API (`RdsDataDb`, with real transactions), and `S3Storage` re-exported from `skyzen-s3`.
- **`skyzen-azure`** (`azure/`) - Azure implementations: Cosmos DB (`CosmosKv`), Blob Storage (`AzureBlob`), Service Bus (`ServiceBusQueue`), Azure Storage queues (`AzureStorageQueue`).

**Tooling:**
- **`skyzen-cli`** (`cli/`) - Unified CLI binary: `new`, `add`, `doctor`, `dev`, `build`, `provision`, `migrate` (+ `migrate status`), `deploy`, `logs`, `secret`, `completions`, with a global `--provider` / `--env` / `--manifest` / `--dry-run`. It links the wasm-bindgen generator and the binaryen optimizer in rather than shelling out, so `cargo install skyzen-cli` is the whole toolchain install. Templates and the generated Worker shim are askama templates under `cli/templates/`; `wrangler.toml` is a `Serialize` model, never string assembly.

---

## Key Design Patterns & Invariants

### 1. Services Abstraction (Two-Trait Pattern)
Services use a two-layer design for dynamic dispatch and type erasure:
- **Public Trait** (e.g. `KeyValueStore`): Uses `impl Future` returns for ergonomic implementors. Not object-safe.
- **Internal Trait** (e.g. `KeyValueStoreObj`): Uses `BoxFuture` returns, making it object-safe for dynamic dispatch via `Box<dyn KeyValueStoreObj>`. It and its blanket bridge are **generated** by the `service_obj!` macro (`services/src/macros.rs`) from the public trait's signatures, so adding a service method is a single-site change. The one exception is `DbTransactionBackendObj`, whose `&mut self` / `self: Box<Self>` shape the macro does not cover.
- **Wrapper** (e.g. `Kv`): Holds `Box<dyn KeyValueStoreObj>`, implements `Extractor`, and exposes high-level async convenience methods (`get_json`, `put_json`, etc.).

*Rule:* Platform crates depend **only** on `skyzen-services` (the trait definitions), **never** on the root `skyzen` crate.

### 2. Service Futures Are `Send` On Every Target
Service wrappers travel in `http::Extensions`, which requires `Send + Sync` unconditionally — including on wasm32 — so there is no target where a `!Send` service future would help. Every service trait therefore writes a plain `Send` bound, with no alias and no `#[cfg]` split.

WebAssembly backends still work because portability is bought where the JS handles live, not in the trait: `skyzen-cloudflare` wraps each `JsValue` handle in a newtype carrying a contained `unsafe impl Send + Sync`, sound because a Workers isolate is single-threaded and never moves the handle across threads, and drives JS promises through `worker::send::IntoSendFuture`. A new wasm backend follows the same recipe.

The old workspace-wide `MaybeSend` alias is **gone**. The name survives in exactly one place, `src/websocket/session.rs`, where `MaybeSend`/`MaybeSync` are `#[cfg]`-split markers bounding a *user's session closure and future* — not a service future. A WebSocket session on wasm is built from `Rc`s and JS handles that no `Send` future could hold across an `await`, and there is no second thread to send it to; the relaxation stops at the route builder, and what the router actually stores still satisfies `http_kit::Endpoint`'s unconditional `Send` by carrying the session in a cell whose `unsafe impl Send` rests on the same single-threaded argument. Do not reintroduce the pattern anywhere else.

### 3. Middleware, Layers & Build-Time Wiring

- `skyzen_core::middleware::Middleware` takes **`&self`**. The router stores each middleware once
  as `Arc<dyn MiddlewareObj>` and never clones it per request, so state kept in an atomic or a
  channel persists. `Next::run(request)` continues the chain and hides the endpoint/middleware
  error split behind `skyzen::Error`.
- The trait lives in `skyzen-core`, not `skyzen`: `skyzen-services`' `service_extractor!` macro
  implements it and only depends on core.
- Three attachment scopes, narrowest first: `RouteNode::with` (one node), `Route::with` (subtree),
  `Route::layer` / `Router::layer` (the whole router, **including the 404/405 paths** — this is what
  makes CORS preflight answerable). Applying middleware pushes it to the front of the endpoint's
  stack, so the last call is outermost.
- `Route::build()` validates wiring: an endpoint whose extractors declare `Extractor::requirements()`
  must have those `TypeId`s in the `provisions()` of middleware on its ancestor chain.
  **Only declare a requirement when route middleware is the sole supply path** — `State<T>` and
  `AuthUser<U>` qualify; the service wrappers do not, because `#[skyzen::main]` and
  `skyzen_test::TestClient` also inject them out of band.

### 4. Handler, Extractor & Responder System
- **Extractors** (`skyzen_core::Extractor`): Types that extract themselves from a `&mut Request`. Tuples of extractors implement `Extractor` automatically, up to arity 15.
- **Responders** (`skyzen_core::Responder`): Types that mutate a `&mut Response`. Tuples of responders compose automatically.
- **Handlers** (`skyzen::handler`): Any async function taking extractors as arguments and returning a responder is automatically converted into an `Endpoint`.
- **The body is taken once.** `skyzen_core::body::take_body_bytes` enforces two invariants for every
  buffering extractor: `RequestBodyLimit` (2 MiB unless a `BodyLimit` middleware says otherwise,
  checked against `Content-Length` *and* mid-stream), and `BodyConsumed`, which poisons the body the
  moment an extractor takes it — including a *failed* one — so a second body extractor in the same
  signature is a loud `500` naming both rather than a silent empty body.

### 5. One Binary, Four Deployments (`#[skyzen::main]`)

Nothing in an application is annotated for a platform. The `wasm32` build is the Worker; the native
build **reads its environment before it binds anything** and hands over accordingly:

| Target | Selected by | What runs |
|---|---|---|
| Native server | nothing else matched | Tokio + Hyper, a `tracing` subscriber, `--port`/`--host`/`--listen`, `Ctrl+C` shutdown |
| AWS Lambda | `AWS_LAMBDA_RUNTIME_API` is set | `skyzen-lambda` (root crate's `lambda` feature). Without the feature the binary **refuses to start** and names it, rather than binding a port nothing in Lambda can reach |
| Azure Functions | `FUNCTIONS_CUSTOMHANDLER_PORT` is set | `src/runtime/azure.rs` mounts the declared `[[azure.queue_triggers]]` in front of the router; HTTP triggers reach the router untouched |
| Cloudflare Workers | `target_arch = "wasm32"` | the WinterCG `fetch` export, plus whatever the manifest implies |

Detection is in `src/runtime/mod.rs`; the adapters are `skyzen-lambda` and `src/runtime/azure.rs`.

**Events are dual-target too.** `#[skyzen::queue]` is one handler driven four ways: Skyzen's own
polling loop (`src/runtime/consumer.rs`) when `[[native.queue_consumer]]` declares one, the
platform's push on Workers and on Lambda, and the Functions host's `POST /{function}` on Azure. The
polling loops deliberately do **not** run under either serverless host — the platform owns delivery
there. `#[skyzen::scheduled]`, `#[skyzen::email]`, `#[skyzen::tail]` and `#[skyzen::durable_object]`
remain Cloudflare-only, because nothing else has those events.

### 5a. Portable SQL and Migrations
- `Db` is one API over sqlx (Postgres/MySQL/SQLite), Cloudflare D1 and the Aurora Data API. `begin`
  is a real transaction on sqlx and the Data API; D1 has none, so `execute_batch` is its atomic unit
  and `begin` returns `DbError::TransactionsUnsupported` there rather than pretending.
- Migrations are `<version>_<name>.sql` files read by **one** reader, `skyzen-manifest`'s
  `migrations` module, so `embed_migrations!` (compile time) and `skyzen migrate` (deploy time)
  cannot disagree about which files count or how a checksum is computed. `Db::migrate` applies each
  file and its bookkeeping row in one `execute_batch`, and refuses to run when an applied file's
  checksum no longer matches.

### 6. Error Handling & Log Security
- `#[skyzen::error]` generates `Display`, `Error`, and `HttpError` implementations with status code attributes.
- **4xx errors** return formatted JSON messages to clients.
- **5xx errors** return generic `"Internal server error"` to prevent leaking internal database/system information to clients while preserving detailed logs on the server.

### 7. WebSockets
- Native backend uses `async-tungstenite`.
- WASM backend uses `WebSocketPair` FFI bindings.
- Both share a unified interface: `socket.next()`, `socket.send_text()`, `socket.send_binary()`, `socket.recv_json::<T>()`, `socket.send(&json)`.

---

## Coding Conventions & Workspace Lints

- **Strict Clippy Lints**: The workspace enforces `pedantic` and `nursery` lint groups. All crates warn on `missing_docs` and `missing_debug_implementations`.
- **Dependencies**: Core HTTP primitives (`Request`, `Response`, `Body`, `Endpoint`) come from `http-kit`. The `Middleware` trait is Skyzen's own, in `skyzen-core`.
- **Formatting**: Run `cargo fmt` before finishing changes.

