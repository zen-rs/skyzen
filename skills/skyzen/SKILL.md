---
name: skyzen
description: >
  Build, optimize, embed, test, and deploy Rust web applications with Skyzen.
  Load whenever writing Skyzen handlers, extractors, responders, middleware,
  error types, or OpenAPI docs; running `skyzen` CLI commands or configuring
  `Skyzen.toml`; deploying to native Linux (systemd + `[native.linux]`),
  Cloudflare Workers, AWS Lambda, or Azure Functions; embedding Skyzen into a
  custom runtime (`Server`, `skyzen-h1`, `skyzen-h2`, `skyzen-hyper`) or using
  `skyzen-core` in `no_std`; tuning Linux `io_uring`, kTLS, busy polling, and
  connection steering; enabling HTTP/2 or HTTP/3; or designing portable
  multi-cloud web applications on `skyzen-services`.
---

# Using the Skyzen Framework

Skyzen is a router-first Rust HTTP framework with portable infrastructure (`skyzen-services`), a thread-per-core completion-based native server (`compio` + `skyzen-h1`/`skyzen-h2` + optional `http-kit-h3`), optional runtime embedding (`Server` trait), a `no_std`-capable core (`skyzen-core`), and one-binary deployment across Native, Cloudflare Workers, AWS Lambda, and Azure Functions.

---

## 1. Framework Core: Routing, Handlers, Extractors, Responders, Errors & Middleware

### Routing & Thread-Per-Core `!Send` Model
- Build routing trees from string literals via `CreateRouteNode` and finalize with `Route::build()` (or `Route::try_build()`):
  ```rust
  use skyzen::routing::{CreateRouteNode, Route, Router};

  fn app() -> Router {
      Route::new((
          "/health".at(health),                        // GET shorthand (there is NO `.get()`)
          "/posts".at(list_posts).post(create_post),
          "/posts/{id}".put(update_post).delete(delete_post),
          "/assets/{*path}".at(serve_asset),
          "/v1".route((                                // child paths are relative to `/v1`
              "/users".at(list_users),
          )),
          "/ws".ws(ws_handler),                        // HTTP/1.1 upgrade, H2 RFC 8441, and WASM
      ))
      .fallback(not_found)
      .method_not_allowed(wrong_method)                // can extract `AllowedMethods`
      .layer(skyzen::middleware::Trace::default())
      .build()
  }
  ```
- Use `"/api".nest(router)` to mount an already-built `Router`, and `"/api".mount(endpoint)` for an arbitrary `Endpoint` (such as `Proxy`).
- **Routers, handlers, and middleware are `!Send` and live on one serving thread.** Never add `Send` bounds to endpoints or middleware; `Rc`, `RefCell`, and `Cell` work directly inside handlers and middleware. What crosses threads is the factory (`EndpointFactory<ServingThread>`), which `#[skyzen::main]` generates automatically (accepting `fn main() -> Router` or `fn main(thread: &ServingThread) -> Router`).

### Handlers & Extractors
- Any `async fn` taking up to **15** `Extractor` arguments and returning a `Responder` (or `skyzen::Result<impl Responder>`) is a handler.
- **Schema bound:** Payloads carried by `Json<T>`, `Form<T>`, and `Query<T>` require `#[derive(ToSchema)]` alongside serde derives (add `utoipa = "5"` in `Cargo.toml`; `skyzen::ToSchema` re-exports the trait). `Path<T>` has no `ToSchema` bound.
- **Body invariants (enforced by `take_body_bytes`):**
  1. Default request body cap is **2 MiB** (`RequestBodyLimit::DEFAULT`), checked against both `Content-Length` and mid-stream chunks. Raise or disable it per subtree with `BodyLimit` / `RequestBodyLimit::disabled()`.
  2. **The body can be consumed only once** (`BodyConsumed`). A second body-consuming extractor in the same handler signature fails loudly with `500` naming both extractors.
- Key built-in extractors:
  - `Path<T>` (`skyzen::extract::Path`) — tuple, struct, or primitive; `Params` (`skyzen::routing::Params`) for dynamic lookup
  - `Query<T>` (`skyzen::extract::Query`) — `serde_html_form`, supports repeated keys (`?tag=a&tag=b` -> `Vec<String>`)
  - `Json<T>`, `Form<T>`, `Multipart`, `CookieJar`, `State<T>` (`skyzen::utils::*`)
  - `Valid<E>` (`validate` feature, `garde`) — wraps `Json`, `Form`, `Query`, `Path` (`Valid(Json(body), _)`); optional second type param `State<Ctx>` supplies validation context
  - `TypedHeader<H>` (`typed-header` feature, re-exports `skyzen::headers`)
  - `BearerToken` (`auth` feature), `ClientIp` + `TrustedProxies`, `PeerAddr`, `Preconditions` (`412`/`428`), `RequestId`, `Session` / `TypedSession<T>`, `IoSettings` (native)
  - `HeaderMap`, `Uri`, `Method`, `String`, `Bytes`, `Body`, `RequestBodyLimit`
  - Wrap any extractor in `Option<T>` or `Result<T, E>` to handle rejections inline.

### Responders
- `&'static str`, `String`, `Bytes`, `StatusCode`, `HeaderMap`, `(HeaderName, HeaderValue)`, `Json<T>`, `PrettyJson<T>`, `Html<T>`, `Redirect`, `CookieJar`, `Sse`, `Conditional<R>` / `Conditional::lazy` (`ETag`/`Last-Modified` -> `304` without serializing `R`), tuples such as `(StatusCode, HeaderMap, Json<T>)`, and `Result<T, E>` where `E: HttpError`.

### Error Handling
- Define domain errors with `#[skyzen::error]` (generates `Display`, `std::error::Error` with `source()`, and `HttpError`):
  ```rust
  #[skyzen::error]
  pub enum AppError {
      #[error("item {0} not found", status = NOT_FOUND)]
      NotFound(u64),
      #[error("invalid field '{field}': {reason}", status = BAD_REQUEST)]
      Validation { field: &'static str, reason: String },
  }
  ```
- Return `skyzen::Result<T>` when mixing multiple failure types:
  - Any `HttpError` (`PathError`, `Json` rejection, `KvError`, `#[skyzen::error]`) converts via `?` and **preserves its status code**.
  - Plain `std::error::Error` types do **not** convert implicitly — state the HTTP status explicitly with `ResultExt`:
    ```rust
    use skyzen::{Context, Result, ResultExt, StatusCode};

    std::env::var("UPSTREAM_KEY")
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .context("reading upstream key")?;
    ```
    Use `.status_msg(status, msg)` to hide the inner error from clients while keeping it in `source()` logs.
- **Client visibility rule:** `4xx` responses return `{"error":"<message>"}` verbatim; `5xx` responses redact the body to `"Internal server error"` and log the full `ErrorChain` server-side.

### Middleware & Build-Time Wiring
- `Middleware` takes **`&self`** and is stored once per thread as `Rc<dyn MiddlewareObj>` (never cloned per request). Keep mutable per-thread state in `Cell`/`RefCell`, or cross-thread state in atomics/channels.
- Attachment scopes (last applied is outermost):
  - `RouteNode::with(m)` — one path node
  - `Route::with(m)` — subtree
  - `Route::layer(m)` / `Router::layer(m)` — entire router **including 404/405 paths** (required for `Cors` preflight, `Trace`, `RequestId`, `SecurityHeaders`).
- `Route::build()` validates `Extractor::requirements()` against ancestor middleware `provisions()` for `State<T>` and `AuthUser<U>`, failing at startup instead of on the first request.
- Shipped middleware (`skyzen::middleware::*`): `Cors`, `Trace`, `RequestId`, `CompressionMiddleware`, `DecompressionMiddleware` (gzip/deflate/br/zstd), `BodyLimit`, `MultipartLimits`, `Timeout` (native only), `Sessions` / `SessionsLayer` (KV, encrypted cookie, or memory; KV and cookie require `CookieKeys` above them), `Csrf`, `RateLimit` (KV or memory), `SecurityHeaders`, `AutoETag`, `ErrorHandlingMiddleware`, `AuthMiddleware`.

### OpenAPI & In-Process Testing
- Annotate handlers with `#[skyzen::openapi]`; mount Scalar UI with `Route::enable_api_doc()` (`/api-docs`) or `routes.openapi().scalar_route("/docs")` + `.json_route("/openapi.json")`. Turn off the default `openapi` feature to strip all metadata at compile time.
- Test without sockets, containers, or `wrangler` using `skyzen-test`:
  - `TestContext::new().with_kv(...).with_db(...).client(app())` (also `with_storage`, `with_queue`, `with_durable_kv`, `with_durable_db`, `with_alarm`).
  - `#[skyzen::test(migrations = MIGRATIONS)]` populates mocks automatically from `Skyzen.toml` and runs migrations.

---

## 2. Skyzen CLI & `Skyzen.toml`

Install prebuilt binaries (preferred) or build from source — the `wasm-bindgen` generator and `wasm-opt` (binaryen) are linked in:
```sh
cargo binstall skyzen-cli
# or: cargo install skyzen-cli
```

| Command | Purpose |
|---|---|
| `skyzen new <name> --template <api\|minimal\|serverless-events\|durable-realtime>` | Scaffold project, `Skyzen.toml`, `.env.example`, `.gitignore` |
| `skyzen add <backend>` (`--list`) | Run `cargo add` for a backend crate/feature (`redis`, `postgres`, `dynamodb`, `s3`, `cloudflare`, …) |
| `skyzen doctor [--provider <p>]` | Verify toolchain, `wasm-bindgen` version parity, `Skyzen.toml`, bindings, auth, env vars, and Linux kernel/NIC acceleration |
| `skyzen dev [--provider native\|cloudflare] [-- <args>]` | Watch & rebuild locally; loads `.env` then `.env.local` (process env wins) |
| `skyzen build --provider <p> [--release]` | Build artifacts; precompresses `[static_files] precompress` dirs; prints raw + gzipped wasm size for Cloudflare |
| `skyzen openapi [--print \| --json <path> \| --no-open]` | Runs binary with `SKYZEN_OPENAPI_DUMP` before service wiring (works with no credentials or `.env`) |
| `skyzen provision --provider cloudflare [--dry-run]` | Create missing KV/D1 resources and write IDs back to `Skyzen.toml` |
| `skyzen migrate [status] [--provider <p>] [--local] [--env <e>]` | Apply or inspect SQL migrations |
| `skyzen deploy --provider <native\|cloudflare\|aws\|azure> [--env <e>] [--dry-run]` | Build/apply and deploy, delivering declared `[[secret]]` and wiring variables |
| `skyzen logs [--provider <p>] [--env <e>] [-- <args>]` | Tail deployed logs |
| `skyzen secret <set NAME \| push \| list> [--provider <p>]` | Manage declared `[[secret]]` values (`set` reads stdin; never passed on argv or printed) |
| `skyzen completions <zsh\|bash\|fish>` | Generate shell completions |

**Three distinct variable mechanisms in `Skyzen.toml`:**
1. `url_env = "CACHE_URL"` — runtime env var name read by the native/Lambda/Functions binary after startup.
2. `"${CACHE_NAMESPACE_ID}"` — deploy-time placeholder expanded by the CLI when reading `Skyzen.toml` (not expanded by `#[skyzen::main]`, so `cargo build` never needs deploy secrets).
3. `[[secret]] name = "STRIPE_KEY"` — portable secret; `import_config!` generates `pub struct StripeKey(Secret)` (`stripe.expose()`, redacted `Debug`, no `Display`), resolved once at startup on every target. Never place secrets in `[cloudflare.vars]` or `[aws.env]`.

---

## 3. Deploying Across Cloud Platforms

One `#[skyzen::main]` binary selects its target automatically at startup (or compile time for `wasm32`):

| Target | Selection | Notes |
|---|---|---|
| **Native server** | Default when no serverless env var is set | Thread-per-core `compio` server |
| **Cloudflare Workers** | `target_arch = "wasm32"` | WinterCG `fetch` export |
| **AWS Lambda** | `AWS_LAMBDA_RUNTIME_API` set | Requires `lambda` feature; refuses to start without it |
| **Azure Functions** | `FUNCTIONS_CUSTOMHANDLER_PORT` set | Custom handler; mounts `[[azure.queue_triggers]]` |

### Native Linux (`systemd`)
1. Declare `[native.server]` (`listen = "0.0.0.0:8080"` or `{ unix = "/run/<bin>/<bin>.sock" }`, optional `busy_poll*`) and optional `[native.linux]` host settings in `Skyzen.toml`.
2. Build as the project owner (never as root, which would leave `target/` root-owned), then deploy:
   ```sh
   skyzen build --provider native --release
   sudo -E env "PATH=$PATH" skyzen deploy --provider native --dry-run
   sudo -E env "PATH=$PATH" skyzen deploy --provider native
   ```
3. `deploy` applies `[native.linux]` via netlink/sysfs/procfs and re-verifies, writes `/etc/skyzen/<bin>.env` (`0600`), installs `/etc/systemd/system/<bin>.service` (`DynamicUser=yes`, `LimitMEMLOCK=infinity`, `ExecStartPre=+… deploy --host-only`, minimal ambient capabilities derived solely from `[native.server]`), and restarts it.

### Cloudflare Workers
- Configure crate as `crate-type = ["cdylib", "rlib"]` and pin `wasm-bindgen` on `wasm32` to the exact version `skyzen doctor` reports.
- Run `skyzen provision --provider cloudflare`, `skyzen migrate`, then `skyzen deploy --provider cloudflare` (add `--upload-only` for gradual `wrangler versions` rollouts; `--env staging` selects `[cloudflare.env.staging]`).
- For Durable Objects, `impl DurableObject` **must** declare `const PERSIST: bool` (`false` when storing state in `DurableDb`/`DurableKv`; `true` commits the struct's serde shape permanently).

### AWS Lambda
- Enable `skyzen = { version = "0.6", features = ["lambda"] }`.
- Configure `[aws]` (`function_name`, `memory_mb`, `timeout`, `architecture = "arm64"`, `url = true`, `[aws.env]`).
- Deploy with `skyzen deploy --provider aws` (runs `cargo lambda build` + `cargo lambda deploy` + `UpdateFunctionConfiguration` for secrets/wiring).
- For SQS triggers, create the event source mapping with `--function-response-types ReportBatchItemFailures` so partial-batch responses work. `[[native.queue_consumer]]` polling loops do not run in Lambda, and `WorkerContext` is unavailable (Lambda freezes immediately after returning).

### Azure Functions
- Configure `[azure]` (`app_name`, `subscription_id`, `resource_group`, `target = "x86_64-unknown-linux-musl"`, `http_mode = "forward"` or `"proxy"` — **use `"proxy"` for SSE or streaming responses**), plus `[[azure.queue_triggers]]` if consuming Storage queues.
- `skyzen deploy --provider azure` refuses non-ELF binaries before publishing and merges secrets/wiring into Function App settings via ARM.

---

## 4. Embedding & `no_std` Usage

### Embedding Skyzen Into an Existing Runtime
When integrating into an existing `tokio`, `smol`, or `compio` process (without `#[skyzen::main]`):
1. Disable `rt` (`default-features = false`) and pick a `Server` implementation:
   - `skyzen-h1::H1` / `skyzen-h2::H2` — owned-buffer HTTP/1.1 and HTTP/2 on any `http-kit-io` driver (`Compio`, `Tokio`, `AsyncIo`).
   - `skyzen::hyper::Hyper` (`hyper` feature) — Hyper HTTP/1.1 (with upgrades) and prior-knowledge HTTP/2 on any `http-kit-io` driver.
2. Install your own `tracing_subscriber` (without `rt`, Skyzen installs none).
3. Build the `!Send` `Router` on the thread that serves it and pass an `http-kit-io` driver, a registered listener, an `executor_core::LocalExecutor`, and a shutdown future to `Server::serve`:
   ```rust
   use std::rc::Rc;
   use async_executor::LocalExecutor;
   use http_kit_io::{async_io::AsyncIo, FromStd as _};
   use skyzen::{hyper::Hyper, routing::{CreateRouteNode, Route}, Server};

   let router = Route::new(("/health".at(|| async { "OK" }),)).build();
   let executor = Rc::new(LocalExecutor::new());
   let listener = AsyncIo.tcp_listener(std::net::TcpListener::bind("127.0.0.1:3000")?)?;
   let serve = Hyper.serve(AsyncIo, Rc::clone(&executor), listener, router, shutdown_fut);
   smol::block_on(executor.run(serve));
   ```
4. If mounting `StaticDir` under a non-compio executor, pass `ServingThread::without_compio()` (and use `BlockingFs`). See `examples/embed_hyper.rs`.

### `no_std` / Embedded Targets (`skyzen-core`)
- Disable default `std` on `skyzen-core`:
  ```toml
  skyzen-core = { version = "0.3", default-features = false }
  ```
- Requires `alloc`; provides `Extractor`, `Responder`, `Middleware`, `RequestBodyLimit` / `BodyConsumed`, and `Error`/`HttpError`/`Result`/`ResultExt`/`Context`.
- `Server`, `upgrade`, `Secret`, Linux `steering`/`busy-poll`, and the root `skyzen` crate require `std` and cannot compile as `no_std`.

---

## 5. Enabling HTTP/2 & HTTP/3

### HTTP/2 (`skyzen-h2`, always included natively)
- **Cleartext (h2c):** Enabled by default (`--server auto` / `SKYZEN_SERVER=auto`) — sniffs the 24-byte HTTP/2 preface over `ReadOwned` without copying on both TCP and `unix:` sockets. Force one protocol with `--server h1` or `--server h2`.
- **Over TLS:** Enable `server-tls`, pass `--tls-cert` and `--tls-key` (`SKYZEN_TLS_CERT` / `SKYZEN_TLS_KEY`). ALPN negotiates `h2` or `http/1.1` automatically over `http-kit-tls` (`rustls`), activating Linux kTLS when supported.
- **WebSockets over HTTP/2:** `.ws(...)` routes automatically accept RFC 8441 Extended `CONNECT` (`:protocol = websocket`) streams on `skyzen-h2`.
- **Outbound HTTP/2:** Enabled automatically on `Proxy` and AWS/Azure HTTP service clients (`zenwave` `native-http2`).

### HTTP/3 (`http3` feature)
- Enable `http3` (implies `server-tls`; builds `quiche`/BoringSSL):
  ```toml
  skyzen = { version = "0.6", features = ["http3"] }
  ```
- Run on a TCP listener with TLS certificates and `--http3` (or `SKYZEN_HTTP3=true`):
  ```sh
  ./target/release/my-app --listen 0.0.0.0:443 \
    --tls-cert cert.pem --tls-key key.pem --http3
  ```
- Binds UDP on the exact same port as TCP using the same PEM pair, advertises `Alt-Svc` on TLS responses, and spawns one QUIC endpoint per serving thread on `SO_REUSEPORT` platforms (Linux/Android; first thread only elsewhere). Passing `--http3` makes any start unable to serve QUIC (missing feature, no TLS, `unix:`, serverless host, UDP bind failure) fail immediately.
- **OpenSSL conflict:** On targets where `native-tls` links OpenSSL (Linux, BSD — everything except Apple and Windows), combining `http3` with `tls-native-tls` is a compile error because quiche's BoringSSL exports the same `SSL_*` symbols. Always use `tls-rustls` for database TLS when `http3` is enabled.

---

## 6. Maximizing Performance (Native `io_uring` Path)

1. **Stay on the native `compio` + `skyzen-h1`/`skyzen-h2` path:** Avoid `skyzen-hyper` in high-throughput services — on `compio`, Hyper reads through `CopyingPoll`, which copies every buffer. `skyzen-h1` and `skyzen-h2` operate directly on owned `io_uring` buffers.
2. **Keep state thread-local:** Accept `thread: &ServingThread` in `#[skyzen::main]` so each core builds its own router. Store per-thread caches/counters in `Rc`/`RefCell`/`Cell` inside middleware or handlers instead of shared mutexes. Let `#[skyzen::main]` rebind zenwave-backed AWS/Azure clients per thread, and build `Proxy::new(&upstream, thread)` inside the per-thread factory so outbound pools share the thread's `io_uring`.
3. **Zero-copy static files & reverse proxy:**
   - Pass `&ServingThread` to `StaticDir::new("/assets", "./public", thread)` so it uses `CompioFs` (batched open+statx in one `io_uring_enter`), lock-free per-thread LRU + `notify` invalidation, `sendfile`/`splice` for large files, and registered fixed buffers (`--fixed-buffers`) for cached files at or above `--zero-copy-threshold`.
   - Enable `[static_files] precompress = ["public"]` in `Skyzen.toml` so `skyzen build` writes `.br`/`.zst`/`.gz` siblings ahead of time.
   - Use `server-tls` on Linux with the `tls` kernel module loaded (`kTLS`) so `sendfile` and `Proxy` socket-to-socket `splice` (HTTP/1.1 bodies, unpadded HTTP/2 `DATA`, and `101` upgraded tunnels) bypass user space even over HTTPS.
4. **Tune Linux `io_uring` & kernel/NIC acceleration:**
   - **Incremental provided buffer ring:** `--recv-buffers <n>` / `--recv-buffer-len <bytes>` (`SKYZEN_RECV_BUFFERS`, `SKYZEN_RECV_BUFFER_LEN`); uses `IOU_PBUF_RING_INC` on Linux 6.12+ so small reads share one buffer.
   - **Registered fixed buffers:** `--fixed-buffers <bytes>` (`SKYZEN_FIXED_BUFFERS`); pins a per-thread region charged to `RLIMIT_MEMLOCK` (needs `LimitMEMLOCK=infinity` or `CAP_IPC_LOCK`).
   - **Zero-copy send threshold & file table:** `--zero-copy-threshold <bytes>` / `--file-table-slots <n>` (`SKYZEN_ZERO_COPY_THRESHOLD`, `SKYZEN_FILE_TABLE_SLOTS`); also live-adjustable at runtime by extracting `IoSettings` in a handler.
   - **NAPI busy polling (Linux 6.9+):** `--busy-poll <µs>` (`SKYZEN_BUSY_POLL`), plus `--busy-poll-prefer` and `--busy-poll-budget <n>` (prefer/raised budget require `CAP_NET_ADMIN`). Registers `IORING_REGISTER_NAPI` on each thread's ring and sets `SO_BUSY_POLL*` on TCP/QUIC sockets.
   - **Connection steering:** `--steering auto|on|off` (`SKYZEN_STEERING`); pins serving threads to CPUs and attaches `SO_ATTACH_REUSEPORT_CBPF` so accepted connections stay on the core handling their NIC RX IRQ.
5. **Verify and apply host facilities declaratively:** Run `skyzen doctor` to audit kTLS, NIC TLS offload, header/data split, busy polling, IRQ placement, steering, and `RLIMIT_MEMLOCK`, and declare `[native.linux]` + `[native.server]` in `Skyzen.toml` for `skyzen deploy --provider native`:
   ```toml
   [native.server]
   listen = "0.0.0.0:443"
   busy_poll = 50
   busy_poll_prefer = true
   busy_poll_budget = 64

   [native.linux]
   kernel_tls = "loaded"
   busy_poll = 50
   busy_read = 50

   [native.linux.nic.eth0]
   tls_hw_tx_offload = true
   tls_hw_rx_offload = true
   tcp_data_split = true
   napi_defer_hard_irqs = 2
   gro_flush_timeout = 200000
   rx_irqs = "serving"
   ```
6. **Trim unused features:** Disable unused default features (`default-features = false, features = [...]`), especially `openapi` on minimal/edge builds.

---

## 7. Building Portable Multi-Cloud Web Applications

1. **Depend on capabilities, never vendor SDKs in handlers:** Ask for `Kv`, `Storage`, `Queue`, `Db` (or generated named wrappers from `[[service]]` / `[[database]]` / `[[secret]]` via `import_config!`). Wire backends per target in `Skyzen.toml` (`[native.service.*]`, `[cloudflare.service.*]`, etc.).
2. **Use portable SQL patterns:**
   - Decode rows with `#[derive(FromRow)]` and column newtypes/enums with `#[derive(Column)]` so types like `Uuid`, `DateTime<Utc>`, and `BigDecimal` decode identically across Postgres, MySQL, SQLite, D1, Aurora Data API, and Azure SQL.
   - Embed migrations with `embed_migrations!("migrations")` and apply with `db.migrate(&MIGRATIONS)` or `skyzen migrate`.
   - **D1 transaction rule:** If targeting Cloudflare D1, use `Db::execute_batch` for atomic multi-statement writes — D1 has no interactive transactions, so `Db::begin` returns `DbError::TransactionsUnsupported`.
3. **Use portable event handlers:** Write queue consumers once with `#[skyzen::queue]`; Skyzen polls natively (`[[native.queue_consumer]]`), and Cloudflare/Lambda/Azure push automatically. Avoid `#[skyzen::scheduled]`, `#[skyzen::email]`, `#[skyzen::tail]`, and `#[skyzen::durable_object]` in code that must run off Cloudflare (or isolate them).
4. **Watch platform-specific boundaries:**
   - **Static assets:** Use `EmbeddedStaticDir` (`include_dir!`) for native + wasm portability; `StaticDir` is native-only.
   - **Middleware:** `Timeout` is native-only. `RateLimit::kv` fails loudly on Cloudflare Workers KV (which lacks atomic increment-with-TTL); use Durable Objects on Workers or `RateLimit::memory` for single-node setups.
   - **Background tasks:** `WorkerContext::wait_until` works natively and on Cloudflare Workers, but is unavailable on AWS Lambda.
   - **WebSockets:** WASM WebSockets enforce a 1 MiB message limit and have no manual ping/pong control.
   - **Streaming on Azure Functions:** Set `[azure] http_mode = "proxy"` when using SSE or streaming responses.
5. **Verify portability in `cargo test`:** Drive the exact same `Router` and handlers with `skyzen-test` (`InMemoryKv`, `InMemoryStorage`, `InMemoryQueue`, `InMemoryDb`, `NativeDurableNamespace`) so business logic, extractors, middleware, and migrations are tested natively without cloud emulators.
