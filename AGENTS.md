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
- **`skyzen-core`** (`core/`) - Foundational traits (`Extractor`, `Responder`, `Server`, `EndpointFactory`) reusable by alternative runtimes, the `upgrade` handover between a server and a WebSocket handler, plus `Secret`, the one runtime representation of a `[[secret]]` value. Supports `no_std` when `std` feature is disabled (`Server`, `upgrade` and `Secret` are `std`-only).
- **`skyzen-macros`** (`macros/`) - Procedural macros: `#[skyzen::main]`, `#[skyzen::error]`, `#[skyzen::openapi]`, `#[skyzen::queue]`, `#[skyzen::scheduled]`, `#[skyzen::email]`, `#[skyzen::tail]`, `#[skyzen::durable_object]`, `#[skyzen::test]`, `#[derive(HttpError)]`, plus the function-like `import_config!` (one named extractor per `[[service]]`, `[[database]]` and `[[secret]]`) and `embed_migrations!`.
- **`skyzen-manifest`** (`manifest/`) - The one typed `Skyzen.toml` schema, consumed by **both** `skyzen-macros` (at compile time) and `skyzen-cli` (at deploy time), so a section can never be accepted by one and rejected by the other. Every struct carries `deny_unknown_fields` and the `type`/`backend` discriminants are enums, so a typo or an unsupported value fails at parse time.
- **`skyzen-hyper`** (`hyper/`) - Hyper server adapter implementing the `Server` trait from `skyzen-core`: HTTP/1 and HTTP/2 (prior-knowledge sniffed), on every `http-kit-io` backend, `compio` included (there hyper reads through the bridge's `CopyingPoll`, which copies). Nothing it drives is `Send`: an HTTP/1 upgrade goes through hyper's `without_shutdown` and the stream it hands back, never `with_upgrades()`, and h2 streams spawn on the `LocalExecutor`. `AltSvc` advertises an HTTP/3 endpoint beside a TLS TCP listener. Its `serve_connection`/`IntoService` are also what the native runtime drives each connection with.
- **`skyzen-h1`** (`h1/`) - Skyzen's own HTTP/1.1 server on `http-kit-h1` and owned-buffer `http-kit-io` streams, so it runs on a completion driver (compio) as well as a readiness one.
- **`skyzen-lambda`** (`lambda/`) - AWS Lambda adapter: HTTP invocations and SQS batches, driven by its own Tokio runtime. Reached through the root crate's optional `lambda` feature, never named by an application.

**Services Abstraction & Testing:**
- **`skyzen-services`** (`services/`) - Platform-agnostic service traits (`KeyValueStore`, `ObjectStorage`, `MessageQueue`, `DbBackend`, `DbTransactionBackend`), type-erased extractors (`Kv`, `Storage`, `Queue`, `Db`, `DurableDb`, `DurableKv`), the portable migration runner (`migrate`), and the queue envelope codec (`queue::envelope`) that two independent crates share.
- **`skyzen-test`** (`test/`) - In-memory mock service implementations (`InMemoryKv`, `InMemoryStorage`, `InMemoryQueue`, `InMemoryDb`), HTTP `TestClient`, `TestContext` with a slot per capability (durable ones included), assertion helpers, and snapshot testing support via `insta`.

**Platform Implementations:**
- **`skyzen-redis`** (`redis/`) - Redis implementation of `KeyValueStore`, atomics included.
- **`skyzen-s3`** (`s3/`) - S3-compatible implementation of `ObjectStorage`, with streaming, ranges, multipart and presigning.
- **`skyzen-cloudflare`** (`cloudflare/`) - Cloudflare Workers implementations for KV, R2, Queues, D1 SQL, Durable Objects, the secrets store, `WorkerContext`/`CfProperties`, and the email/tail events (**wasm32-only**).
- **`skyzen-cloudflare-admin`** (`cloudflare-admin/`) - The shared Cloudflare control-plane types: the `{success, errors, result}` response envelope and `TokenSource`. No HTTP transport and no endpoint wrappers, and nothing in this workspace depends on it — `skyzen provision` drives wrangler.
- **`skyzen-aws`** (`aws/`) - AWS implementations: DynamoDB (`DynamoKv`), SQS (`SqsQueue`, FIFO and consume side), the Aurora Data API (`RdsDataDb`, with real transactions), and `S3Storage` re-exported from `skyzen-s3`.
- **`skyzen-azure`** (`azure/`) - Azure implementations: Cosmos DB (`CosmosKv`), Blob Storage (`AzureBlob`), Service Bus (`ServiceBusQueue`), Azure Storage queues (`AzureStorageQueue`).

**Tooling:**
- **`skyzen-cli`** (`cli/`) - Unified CLI binary: `new`, `add`, `doctor`, `dev`, `build`, `provision`, `migrate` (+ `migrate status`), `deploy`, `logs`, `secret` (`set` / `push` / `list`, on Cloudflare, AWS and Azure), `completions`, with a global `--provider` / `--env` / `--manifest` / `--dry-run`. `environment.rs` is the one resolver for the variables a running application needs — `[[secret]]` entries and native wiring — and each provider's module is the one sink that delivers them. It links the wasm-bindgen generator and the binaryen optimizer in rather than shelling out, so `cargo install skyzen-cli` is the whole toolchain install. Templates and the generated Worker shim are askama templates under `cli/templates/`; `wrangler.toml` is a `Serialize` model, never string assembly.

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

### 2a. Routers Are Per-Thread and `!Send`

Endpoints, middleware, handlers and the state they capture never cross a thread, so none of them is bound by `Send` or `Sync`: a handler may hold an `Rc` or a `RefCell`. `Route`/`Router` store the *local* erased forms — `http_kit::endpoint::AnyEndpoint` and `Rc<dyn MiddlewareObj>` — and every generic bound in the router, `Layered`, `Server::serve` and the adapters is `Endpoint + Clone + 'static` with a non-`Send` future. Do not add `SendEndpoint` bounds back; the router is not one.

What crosses threads is the recipe. `skyzen_core::EndpointFactory` (`Send + Sync`, blanket-implemented for `Fn() -> impl Future<Output = E>`) is handed to each serving thread, which builds its own router from it and gives that router, with its listeners and a thread-local `executor_core::LocalExecutor`, to `Server::serve`. `#[skyzen::main]` generates the factory: service clients are built once and *cloned* into each router (they are `Send + Sync`), and `!Send` per-thread state reaches handlers through the router the factory builds — never through `http::Extensions`. Because a `!Send` router cannot sit in `Extensions`, a router is not an extractor.

An adapter whose host requires a `Send` future (lambda_http's `Adapter`) runs the router on a `LocalSet` and hands the host only the `JoinHandle`.

**The upgrade handover.** An upgraded connection is `!Send` on a thread-per-core driver and so cannot travel in `Extensions` either. The server inserts the `skyzen_core::upgrade::UpgradeAvailable` marker on requests that can upgrade (HTTP/1); `WebSocketUpgrade` refuses with `426` without it. The handler answers `101` with an `OnUpgrade` token in the *response* extensions — a one-slot channel carrying the `Send` session closure — and the server, once the connection is handed back (`ServerOutcome::Upgraded` in `skyzen-h1`, hyper's upgrade future in `skyzen-hyper`), runs that session on its own thread over the erased `Upgraded` stream, replaying any bytes read past the handshake.

`MaybeSend` survives in exactly one place, `src/websocket/session.rs`: it bounds a *user's WebSocket session closure* — `Send` natively, because the token carrying it sits in `Extensions`; nothing on wasm, where a session is built from `Rc`s and JS handles and `on_upgrade` starts it with `wasm_bindgen_futures::spawn_local` on the `WebSocketPair` it already holds. Do not reintroduce the pattern anywhere else.

### 3. Middleware, Layers & Build-Time Wiring

- `skyzen_core::middleware::Middleware` takes **`&self`** and returns a non-`Send` future. The
  router stores each middleware once as `Rc<dyn MiddlewareObj>` (boxed futures are
  `LocalBoxFuture`) and never clones it per request, so state kept in a `Cell`, an atomic or a
  channel persists for that thread's router. `Next::run(request)` continues the chain and hides the endpoint/middleware
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
| Native server | nothing else matched | One compio runtime per core (`available_parallelism`), each with its own listener (`SO_REUSEPORT` on Linux, one shared socket elsewhere) and its own router built from the factory on that thread, HTTP/1.1 through `skyzen-h1`; a `tracing` subscriber, `--port`/`--host`/`--listen` (`unix:` served by the same threads), `Ctrl+C` fanned out to every thread. Cleartext is `skyzen-h1` unless `--server hyper`/`SKYZEN_SERVER=hyper` picks Hyper (h2c; not on `unix:`); with `server-tls`, TLS is `http-kit-tls` (rustls) over the compio stream directly, with kernel TLS on Linux, and ALPN choosing the protocol: `h2` is Hyper's HTTP/2, anything else is HTTP/1.1 on the selected server (`skyzen-h1` by default); with `http3` (implies `server-tls`), `http-kit-h3` serves HTTP/3 on UDP beside the TLS listener (`src/runtime/native/http3.rs`): same port, same PEM pair (read once by `tls.rs`), one QUIC endpoint per serving thread on `SO_REUSEPORT` platforms and on the first thread alone elsewhere, drained by the same stop signal, and TLS responses carry `Alt-Svc`. `--http3`/`SKYZEN_HTTP3` makes a start that cannot serve it (no feature, no TLS, `unix:`, a serverless host, UDP bind failure) an error |
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
- Native SQLite (`services/src/sql/sqlite.rs`) holds exactly one read-write connection, so writers
  queue in the pool's FIFO instead of starving in SQLite's polling busy handler; in WAL mode a
  read-only pool takes the statements sqlparser proves are reads. No constructor accepts a
  caller-built pool, because one with several writers brings `database is locked` back.
- Migrations are `<version>_<name>.sql` files read by **one** reader, `skyzen-manifest`'s
  `migrations` module, so `embed_migrations!` (compile time) and `skyzen migrate` (deploy time)
  cannot disagree about which files count or how a checksum is computed. `Db::migrate` applies each
  file and its bookkeeping row in one `execute_batch`, and refuses to run when an applied file's
  checksum no longer matches.

### 5b. Secrets
- A secret is one model on every target: `[[secret]] name = "NAME"` declares it, `import_config!`
  generates `NameInPascalCase` wrapping `skyzen::Secret` (`STRIPE_KEY` → `StripeKey`), and
  `#[skyzen::main]` **resolves it once at startup** — `std::env::var` natively, `CfSecret::classic`
  or `CfSecret::from_store` on Workers when `[cloudflare.secret.<NAME>]` backs it. There is no bare
  `Secret` extractor: the type names a value, not a capability. A test layers one in with
  `router.layer(StripeKey::new(Secret::new("x")))`.
- `Secret` is readable only through `expose`, its `Debug` is `Secret(<redacted>)`, and it has no
  `Display`.
- **One identifier rule.** Every variable name in the schema — a secret's name and every `*_env`
  wiring key — is a `VarName`, validated `[A-Za-z_][A-Za-z0-9_]*` at parse time, so the CLI and the
  macro cannot read different variables from the same key.
- **No value on argv or stdout.** The CLI carries values in `SecretString`, delivers them on a
  child process's standard input (wrangler) or in a request body (Lambda, ARM), writes value-bearing
  files 0600, and prints `write <path> (secret values, not shown)` under `--dry-run`. A leak needs a
  visible `expose_secret()`, which is what a review looks for. The canonical description, including
  the `.env` precedence rule and the per-provider delivery table, is
  `docs/skyzen-toml-reference.md#secrets`.

### 5c. OpenAPI
- The document is target-independent. `#[skyzen::openapi]` collects a `HandlerSpec` per handler and
  `describe_handler` resolves it by `type_name` when the route is built — on a server, in Lambda, in
  Azure Functions and inside a Worker alike, in release as in debug.
- **The `openapi` cargo feature is the only switch**, and the decision is made in `skyzen-macros`,
  not in the expansion. Macro output lands in the *application's* crate, where
  `cfg(feature = "openapi")` would ask about the application's features; so `skyzen/openapi` enables
  `skyzen-macros/openapi` and `expand_openapi_fn` checks `cfg!(feature = "openapi")` at expansion
  time. Feature off ⇒ nothing is emitted at all. This replaced `debug_assertions`, which used to
  stand in for a switch that was not readable and left release builds with an empty document.
- **The document names the application, and no API carries that name.** `env!("CARGO_PKG_NAME")`
  expands where it is written, so `OpenApi::default_info` used to title every user's document
  `skyzen 0.2.1`. The fix is not an argument on `enable_api_doc`: `#[skyzen::main]` expands in the
  application's crate, so it registers an `AppInfo` into `registry::APP_INFO` exactly as
  `#[skyzen::openapi]` registers a `HandlerSpec`, and `OpenApi::info` reads it. One per binary,
  because one attribute defines a binary. `OpenApi::with_info` overrides it; with neither, the title
  is an anonymous `API 0.0.0`, since naming skyzen is worse. **Do not add a name parameter to a
  routing method** — that was tried and reverted; the registry is why it is not needed.
- **`skyzen openapi` runs the application, and that is the only way.** Only the compiled binary
  knows what was collected. `SKYZEN_OPENAPI_DUMP` (defined once in `skyzen-core`, so the CLI and the
  runtime cannot spell it differently) makes `#[skyzen::main]` print the document and exit *before*
  the factory's service wiring — which is what lets the command work with no credentials, no
  reachable backend and an incomplete `.env`, where `skyzen dev` refuses to start. Keep the dump
  ahead of the wiring; moving it after would quietly reintroduce that requirement.
- **`src/openapi/registry.rs` is the only file that asks what target it is.** Native registers into a
  `linkme` distributed slice — linker-section data, nothing before `main`, free at runtime. wasm32
  has no such linker support, so it uses `inventory`: one life-before-main constructor per handler,
  pushing onto a list at isolate start. The cost difference is the entire reason for two backends,
  and `__register_handler_spec!` hides the shape difference from the macro. Do not spread either
  crate's name beyond that file, and do not adopt `inventory` natively.

### 5d. Static Files
- `StaticDir` (native only) keeps a cache **per serving thread, per mount**, inside the endpoint the
  thread's router owns: `Rc`/`RefCell`, no lock. Each cache owns a `notify` watcher whose thread
  bumps an atomic generation on any non-access event; a lookup that sees it move drops the whole
  cache, and a load that raced a change is served but not cached. A directory that cannot be
  watched panics at router build — an unwatched cache would serve stale files.
- `StaticCacheSettings` (entries, inline limit) is the operator's runtime-adjustable handle, shared
  by every clone across threads; a change empties and resizes each cache at its next lookup. No
  ceiling guards it.
- Files below the inline limit are cached as `Bytes`; larger ones as an open descriptor, sent as a
  `FileRegion` (a `dup` per response, since `FileRegion` owns its `File`) so the HTTP/1.1 server
  hands it to `SendFile` — `splice`/`sendfile`, and kernel TLS on Linux.
- The filesystem is a sealed `FileSystem` type parameter: `CompioFs` (default; open + statx pushed
  together, one `io_uring_enter`) or `BlockingFs` for a router driven by another executor.
- `Accept-Encoding` is read by one parser, `utils::accept_encoding`, shared with
  `CompressionMiddleware`. Siblings (`.br`, `.zst`, `.gz`) are written by `skyzen build` from
  `[static_files] precompress` (`cli/src/precompress.rs`), before compilation; a sibling older than
  its file is skipped with a warning.

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

