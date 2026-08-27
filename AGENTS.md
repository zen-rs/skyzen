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
- **`skyzen-macros`** (`macros/`) - Procedural macros: `#[skyzen::main]`, `#[skyzen::error]`, `#[skyzen::openapi]`, `#[skyzen::queue]`, `#[skyzen::scheduled]`, `#[skyzen::email]`, `#[skyzen::tail]`, `#[skyzen::durable_object]`, `#[derive(HttpError)]`.
- **`skyzen-manifest`** (`manifest/`) - The one typed `Skyzen.toml` schema, consumed by **both** `skyzen-macros` (at compile time) and `skyzen-cli` (at deploy time), so a section can never be accepted by one and rejected by the other. Every struct carries `deny_unknown_fields` and the `type`/`backend` discriminants are enums, so a typo or an unsupported value fails at parse time.
- **`skyzen-hyper`** (`hyper/`) - Hyper server adapter implementing the `Server` trait from `skyzen-core`.

**Services Abstraction & Testing:**
- **`skyzen-services`** (`services/`) - Platform-agnostic service traits (`KeyValueStore`, `ObjectStorage`, `MessageQueue`, `DbBackend`) and type-erased extractors (`Kv`, `Storage`, `Queue`, `Db`, `DurableDb`, `DurableKv`).
- **`skyzen-test`** (`test/`) - In-memory mock service implementations (`InMemoryKv`, `InMemoryStorage`, `InMemoryQueue`, `InMemoryDb`), HTTP `TestClient`, assertion helpers, and snapshot testing support via `insta`.

**Platform Implementations:**
- **`skyzen-redis`** (`redis/`) - Redis implementation of `KeyValueStore`.
- **`skyzen-s3`** (`s3/`) - S3-compatible implementation of `ObjectStorage`.
- **`skyzen-cloudflare`** (`cloudflare/`) - Cloudflare Workers implementations for KV, R2, Queues, D1 SQL, and Durable Objects (**wasm32-only**).
- **`skyzen-cloudflare-admin`** (`cloudflare-admin/`) - Cloudflare REST API client for administrative resource management and provisioning.
- **`skyzen-aws`** (`aws/`) - AWS implementations: DynamoDB (`DynamoKv`), S3 (`S3Storage`), SQS (`SqsQueue`).
- **`skyzen-azure`** (`azure/`) - Azure implementations: Cosmos DB (`CosmosKv`), Blob Storage (`AzureBlob`), Service Bus (`ServiceBusQueue`).

**Tooling:**
- **`skyzen-cli`** (`cli/`) - Unified CLI binary: `new`, `add`, `doctor`, `dev`, `build`, `provision`, `deploy`, `logs`, `secret`, `completions`. It links the wasm-bindgen generator and the binaryen optimizer in rather than shelling out, so `cargo install skyzen-cli` is the whole toolchain install. Templates and the generated Worker shim are askama templates under `cli/templates/`; `wrangler.toml` is a `Serialize` model, never string assembly.

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
- **Extractors** (`skyzen_core::Extractor`): Types that extract themselves from a `&mut Request`. Tuples of extractors implement `Extractor` automatically.
- **Responders** (`skyzen_core::Responder`): Types that mutate a `&mut Response`. Tuples of responders compose automatically.
- **Handlers** (`skyzen::handler`): Any async function taking extractors as arguments and returning a responder is automatically converted into an `Endpoint`.

### 5. Dual-Target Runtime (`#[skyzen::main]`)
- **Native**: Sets up a Tokio runtime, installs a `tracing` subscriber, parses CLI args (`--port`, `--host`, `--listen`), and handles graceful shutdown on `Ctrl+C`.
- **WASM**: Generates the WinterCG `fetch` export. Cloudflare Workers events use dedicated macros: `#[skyzen::queue]`, `#[skyzen::scheduled]`, and `#[skyzen::durable_object]`.

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

