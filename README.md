# Skyzen

[![crates.io](https://img.shields.io/crates/v/skyzen.svg)](https://crates.io/crates/skyzen)
[![doc.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen)
[![License](https://img.shields.io/crates/l/skyzen.svg)](#license)

Skyzen is an HTTP framework for Rust that compiles to a native Tokio/Hyper server or to a WebAssembly edge handler (such as Cloudflare Workers) from the same codebase.

Write handlers against portable service wrappers (`Kv`, `Storage`, `Queue`, `Db`), compose routing trees declaratively, and switch between native servers and serverless edge runtimes without rewriting application logic.

---

## Table of Contents

- [Quick Start](#quick-start)
- [Routing](#routing)
- [Handlers, Extractors & Responders](#handlers-extractors--responders)
- [Error Handling](#error-handling)
- [Middleware & State](#middleware--state)
- [Portable Services](#portable-services)
- [WebSockets](#websockets)
- [Static Files & SPA Support](#static-files--spa-support)
- [OpenAPI & Documentation](#openapi--documentation)
- [Testing](#testing)
- [Running on the Edge (Cloudflare Workers)](#running-on-the-edge-cloudflare-workers)
- [Skyzen CLI](#skyzen-cli)
- [Crates Overview](#crates-overview)
- [Guides & Examples](#guides--examples)
- [License](#license)

---

## Quick Start

Add `skyzen` to your `Cargo.toml`:

```toml
[dependencies]
skyzen = "0.1"
serde = { version = "1.0", features = ["derive"] }
```

Write your entry point in `src/main.rs`:

```rust
use serde::{Deserialize, Serialize};
use skyzen::{
    extract::Query,
    routing::{CreateRouteNode, Params, Route, Router},
    utils::Json,
    Result,
};

#[derive(Serialize)]
struct MessageResponse {
    message: String,
}

#[derive(Deserialize)]
struct GreetingQuery {
    prefix: Option<String>,
}

async fn health() -> &'static str {
    "OK"
}

async fn greet_user(
    params: Params,
    Query(query): Query<GreetingQuery>,
) -> Result<Json<MessageResponse>> {
    let name = params.get("name")?;
    let prefix = query.prefix.as_deref().unwrap_or("Hello");
    Ok(Json(MessageResponse {
        message: format!("{prefix}, {name}!"),
    }))
}

fn app() -> Router {
    Route::new((
        "/health".at(health),
        "/greet/{name}".get(greet_user),
    ))
    .build()
}

#[skyzen::main]
fn main() -> Router {
    app()
}
```

Run locally:

```sh
# Start the server (defaults to an open localhost port or reads PORT / SKYZEN_ADDRESS)
cargo run

# Bind a specific port
cargo run -- --port 8080
```

---

## Routing

Skyzen builds routing trees directly from string path literals using the `CreateRouteNode` trait. Route matching uses a radix tree powered by [`matchit`](https://crates.io/crates/matchit).

### Method Builders

Path literals support HTTP method shorthands:

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};

fn router() -> Router {
    Route::new((
        "/".at(home),                     // GET shorthand
        "/posts".get(list_posts),         // Explicit GET
        "/posts".post(create_post),       // POST
        "/posts/{id}".put(update_post),   // PUT
        "/posts/{id}".patch(patch_post),  // PATCH
        "/posts/{id}".delete(delete_post),// DELETE
        "/ws".ws(chat_websocket),         // WebSocket upgrade
    ))
    .build()
}
```

### Path Parameters and Wildcards

- `{name}` matches a single path segment.
- `{*path}` matches the remainder of the path (wildcard).

`Path<T>` deserializes the captured segments into a tuple, a struct, or a single primitive, so a
malformed segment is a `400` naming it rather than a `.parse()` in every handler. `Params` remains
the escape hatch for names only known at runtime.

```rust
use skyzen::extract::Path;
use skyzen::routing::{CreateRouteNode, Params, Route};
use skyzen::Result;

async fn get_user_post(Path((user_id, post_id)): Path<(String, u64)>) -> Result<String> {
    Ok(format!("User {user_id}, Post {post_id}"))
}

async fn serve_asset(params: Params) -> Result<String> {
    let filepath = params.get("path")?;
    Ok(format!("Serving asset: {filepath}"))
}

let routes = Route::new((
    "/users/{user_id}/posts/{post_id}".get(get_user_post),
    "/assets/{*path}".get(serve_asset),
));
```

### Nested Route Trees

Group sub-paths using `.route(...)`:

```rust
let api_routes = Route::new((
    "/v1".route((
        "/users".get(list_users).post(create_user),
        "/users/{id}".get(get_user).delete(delete_user),
    )),
    "/v2".route((
        "/users".get(list_users_v2),
    )),
));
```

---

## Handlers, Extractors & Responders

A handler is an `async fn` whose arguments implement `Extractor` and whose return type implements `Responder`. Handlers do not require macro decoration or manual registration.

```rust
use skyzen::{
    extract::{BearerToken, ClientIp, Path, Query},
    utils::{Json, State},
    Result, StatusCode,
};

async fn update_profile(
    Path(user_id): Path<u64>,
    token: BearerToken,
    ip: ClientIp,
    State(db): State<DatabasePool>,
    Json(payload): Json<ProfileUpdate>,
) -> Result<(StatusCode, Json<UserProfile>)> {
    let profile = db.update_user(user_id, payload).await?;
    Ok((StatusCode::OK, Json(profile)))
}
```

### Built-in Extractors

| Extractor | Type | Description |
|---|---|---|
| `Json<T>` | `skyzen::utils::Json` | Deserializes a JSON request body (`T: DeserializeOwned`) |
| `Query<T>` | `skyzen::extract::Query` | Deserializes URL query parameters |
| `Form<T>` | `skyzen::utils::Form` | Deserializes `application/x-www-form-urlencoded` form bodies |
| `Path<T>` | `skyzen::extract::Path` | Deserializes the route's captured `{name}` segments into a struct, tuple, or primitive |
| `Params` | `skyzen::routing::Params` | Accesses path parameters by name at runtime (`params.get("id")?`) |
| `Multipart` | `skyzen::utils::Multipart` | Streams multipart form data and file uploads |
| `State<T>` | `skyzen::utils::State` | Extracts shared state attached to the route via `.with(State(...))` |
| `BearerToken` | `skyzen::extract::BearerToken` | Extracts the bearer token from the `Authorization` header |
| `ClientIp` | `skyzen::extract::ClientIp` | Resolves the client IP (supporting `X-Forwarded-For` and `CF-Connecting-IP`) |
| `Kv`, `Storage`, `Queue`, `Db` | `skyzen_services::*` | Portable cloud services injected into the request context |
| `Body`, `Bytes`, `ByteStr` | `skyzen::http_kit::*` | Raw request body representations |
| `HeaderMap`, `Uri`, `Method` | `skyzen::http_kit::*` | HTTP request metadata |
| `TypedHeader<H>` | `skyzen::extract::TypedHeader` | One RFC-typed header via the `headers` crate (requires the `typed-header` feature) |

### Built-in Responders

| Responder | Description |
|---|---|
| `&'static str`, `String`, `Bytes` | Plain text or raw byte payload |
| `Json<T>` / `PrettyJson<T>` | Serializes `T` to `application/json` |
| `StatusCode` | Returns an empty response with the given status code |
| `(StatusCode, T)` | Pairs an explicit HTTP status code with any responder `T` |
| `(HeaderMap, T)` | Sets custom response headers alongside a responder `T` |
| `Result<T, E>` | Returns `T` on `Ok`, or maps `E: HttpError` to an HTTP error response |
| `Redirect` | `skyzen::utils::Redirect` — 302 (`to`), 303 (`see_other`), 307 (`temporary`), 308 (`permanent`), or any status via `with_status` |
| `Html<T>` | Sends its payload as `text/html; charset=utf-8` |
| `Sse` | Streams Server-Sent Events |

---

## Error Handling

Define typed application errors with `#[skyzen::error]`. This macro implements `std::error::Error` (including `source()` for `#[from]`/`#[source]` fields), `Display`, and `HttpError`, mapping each variant to an HTTP status code with message formatting:

```rust
use skyzen::{error, StatusCode};

#[skyzen::error]
pub enum AppError {
    #[error("item with id {0} was not found", status = NOT_FOUND)]
    NotFound(u64),

    #[error("validation error on field '{field}': {reason}", status = BAD_REQUEST)]
    Validation {
        field: &'static str,
        reason: String,
    },

    #[error("unauthorized access", status = StatusCode::UNAUTHORIZED)]
    Unauthorized,

    #[error("upstream service timeout", status = GATEWAY_TIMEOUT)]
    Timeout,
}
```

### Mixing Error Types with `skyzen::Result`

A handler that fails in more than one way returns `skyzen::Result<T>`. Any error implementing `HttpError` — a route-parameter rejection, a `Json` rejection, a `KvError`, your own `#[skyzen::error]` enum — converts into it with `?` **and keeps its status**:

```rust
use skyzen::{routing::Params, Result};
use skyzen_services::Kv;

async fn read_profile(params: Params, kv: Kv) -> Result<String> {
    let id = params.get("id")?;              // 400 if the parameter is missing
    let raw = kv.get_text(id).await?;        // 500 if the store is unreachable
    raw.ok_or_else(|| skyzen::Error::msg("no such profile").set_status(skyzen::StatusCode::NOT_FOUND))
}
```

Errors with no HTTP meaning of their own do not convert implicitly — that is deliberate, since guessing a status is how a client error becomes a 500. State one instead:

```rust
use skyzen::{Context, Result, ResultExt, StatusCode};

async fn read_api_key() -> Result<String> {
    // `.status(...)` states the status; `.context(...)` adds a breadcrumb and keeps it.
    std::env::var("UPSTREAM_API_KEY")
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .context("reading the upstream API key")
}
```

`ResultExt::status_msg` and `Option::status`/`status_msg` cover the same ground when the failure has no error value at all.

### Error Security & Behavior

- **4xx Errors**: Return the formatted message to the client in a JSON payload: `404 {"error":"item with id 42 was not found"}`.
- **5xx Errors**: Mask the response body with `"Internal server error"` to prevent internal system or database details from leaking to clients, while preserving the full error message — and its whole `source()` chain — in the server log.

---

## Middleware & State

Attach shared state or middleware to any route branch using `.with(...)`:

```rust
use std::sync::Arc;
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::State,
};

#[derive(Clone)]
struct AppConfig {
    api_key: String,
}

async fn handler(State(config): State<Arc<AppConfig>>) -> String {
    format!("Config loaded with key length {}", config.api_key.len())
}

fn router() -> Router {
    let config = Arc::new(AppConfig {
        api_key: "secret".into(),
    });

    Route::new((
        "/info".at(handler),
    ))
    .with(State(config))
    .build()
}
```

### Writing middleware

A middleware is a value that sees every request on the way in and every response on the way out.
It takes `&self` and is shared across requests, so state kept in an atomic or a channel really
persists:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use skyzen::{middleware::{Middleware, Next}, Error, Request, Response};

#[derive(Debug, Default)]
struct CountRequests {
    seen: AtomicUsize,
}

impl Middleware for CountRequests {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        self.seen.fetch_add(1, Ordering::Relaxed);
        next.run(request).await
    }
}
```

For one-off behaviour, `middleware::from_fn` takes a closure returning a boxed future:

```rust
use skyzen::middleware::from_fn;

let log = from_fn(|request, next| {
    Box::pin(async move {
        tracing::info!(path = request.uri().path(), "request received");
        next.run(request).await
    })
});
```

### Attachment scopes

| Call | Covers |
|---|---|
| `RouteNode::with(m)` | one path node's endpoints |
| `Route::with(m)` / `Route::middleware(m)` | every endpoint in the subtree |
| `Route::layer(m)` / `Router::layer(m)` | the entire router, **including its 404 and 405 responses** |

CORS, tracing and request-id middleware belong on `layer`: a preflight `OPTIONS` arrives at a path
whose registered methods are `GET`/`POST`, so it has to be answered before the router synthesizes
its 405.

### Shipped middleware

| Middleware | Purpose |
|---|---|
| `Cors` | Answers preflight requests and decorates cross-origin responses; rejects credentials + wildcard origin at construction |
| `CompressionMiddleware` | gzip/deflate negotiation, skipping HEAD and unknown-length streams |
| `BodyLimit` | Publishes the `RequestBodyLimit` extension (2 MiB by default); the extension is the contract body extractors are to enforce, which is not yet wired up |
| `Timeout` | Abandons a request that outruns its budget with `408` (native targets only) |
| `ErrorHandlingMiddleware` | Renders endpoint errors into responses |
| `AuthMiddleware` | Authenticates the request and injects `AuthUser<U>` |
| `State<T>`, `Kv`, `Storage`, `Queue`, `Db` | Inject a value the matching extractor reads back |

### Custom 404 and 405 responses

`Route::fallback` and `Route::method_not_allowed` take ordinary handlers, and both run inside the
router's layers. The method-not-allowed handler can read the registered methods back with the
`AllowedMethods` extractor:

```rust
use skyzen::{routing::{AllowedMethods, CreateRouteNode, Route}, Method, Result, Uri};

async fn not_found(uri: Uri) -> Result<String> {
    Ok(format!("no such page: {}", uri.path()))
}

async fn wrong_method(allowed: AllowedMethods) -> Result<String> {
    Ok(format!("try one of {:?}", allowed.methods()))
}

let router = Route::new(("/items".at(|| async { Result::Ok("[]") }),))
    .fallback(not_found)
    .method_not_allowed(wrong_method)
    .build();
```

### Wiring checked at build time

`Route::build()` walks the tree and fails if a handler extracts a `State<T>` or `AuthUser<U>` that
no middleware on its route provides, naming the path and the call that would fix it — instead of
returning a 500 on the first request that reaches the endpoint. `Route::try_build()` returns the
`RouteBuildError` rather than panicking, and `Router::routes()` lists every registered
`(method, path)` for introspection.

---

## Portable Services

The `skyzen-services` crate provides four capability wrappers: `Kv`, `Storage`, `Queue`, and `Db`. Handlers accept these wrappers directly as extractors, keeping business logic decoupled from concrete infrastructure providers.

```rust
use skyzen::{routing::Params, utils::Json, Result};
use skyzen_services::{Db, Kv, Storage};

#[derive(serde::Serialize, serde::Deserialize)]
struct FileMetadata {
    filename: String,
    size: usize,
}

async fn save_file(
    kv: Kv,
    storage: Storage,
    db: Db,
    Json(meta): Json<FileMetadata>,
) -> Result<&'static str> {
    // Write metadata to Key-Value store
    kv.put_json(&format!("meta:{}", meta.filename), &meta).await?;

    // Record entry in relational database
    db.query("INSERT INTO files (name, size) VALUES (?, ?)")
        .bind(&meta.filename)
        .bind(meta.size as i64)
        .execute()
        .await?;

    Ok("saved")
}
```

### Provider Compatibility Matrix

| Capability | Native Server | Cloudflare Workers | AWS | Azure | In-Memory (Tests) |
|---|---|---|---|---|---|
| **Key-Value** (`Kv`) | [`skyzen-redis`](redis/) | `CfKv` | `DynamoKv` | `CosmosKv` | `InMemoryKv` |
| **Object Storage** (`Storage`) | [`skyzen-s3`](s3/) | `CfR2` | `S3Storage` | `AzureBlob` | `InMemoryStorage` |
| **Message Queue** (`Queue`) | [`SqsQueue`](aws/) | `CfQueue` | `SqsQueue` | `ServiceBusQueue` | `InMemoryQueue` |
| **SQL Database** (`Db`) | `Db` via SQLx (Postgres/MySQL/SQLite) | `Db` via D1 | Planned | Planned | `InMemoryDb` (SQLite) |

The **Native Server** column names what a native deployment can wire, not a separate implementation:
runtime and provider are independent axes, so a backend that is a plain HTTP client — `SqsQueue`,
`DynamoKv`, `S3Storage` — works just as well from a native server as from anywhere else. `SqsQueue`
appears twice for that reason, and it is what `[native.service.*]` wiring in `Skyzen.toml` builds for
`backend = "sqs"`. Use `InMemoryQueue` for local development.

### Wiring Backends

Backends can be configured declaratively via [`Skyzen.toml`](docs/skyzen-toml-reference.md) or wired manually in code:

```rust
// Native (Redis + S3)
let kv = Kv::new(Redis::connect("redis://127.0.0.1:6379").await?);
let storage = Storage::new(S3Storage::from_env("my-bucket"));

// Cloudflare Workers (KV + R2 bindings)
let kv = Kv::new(CfKv::from_env(&env, "KV_BINDING")?);
let storage = Storage::new(CfR2::from_env(&env, "R2_BINDING")?);

// Unit & Integration Tests (No external dependencies)
let kv = Kv::new(InMemoryKv::new());
let storage = Storage::new(InMemoryStorage::new());
```

See the [Services Guide](docs/services-guide.md) and [Durable Object + SQL Guide](docs/durable-sql-guide.md) for deeper coverage.

---

## WebSockets

Skyzen provides a unified WebSocket API that compiles to `async-tungstenite` on native runtimes and `WebSocketPair` on WebAssembly edge runtimes.

```rust
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::websocket::WebSocketMessage;

#[derive(Serialize, Deserialize)]
struct ChatPayload {
    user: String,
    text: String,
}

fn router() -> Router {
    Route::new((
        // Shorthand WebSocket endpoint
        "/ws".ws(|mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                if let Some(text) = message.into_text() {
                    let _ = socket.send_text(format!("Echo: {text}")).await;
                }
            }
        }),
        // Strongly typed JSON messages
        "/ws/json".ws(|mut socket| async move {
            while let Some(Ok(chat)) = socket.recv_json::<ChatPayload>().await {
                let reply = ChatPayload {
                    user: "server".into(),
                    text: format!("Hello, {}", chat.user),
                };
                let _ = socket.send(&reply).await;
            }
        }),
    ))
    .build()
}
```

*Note: WASM WebSockets enforce a 1 MiB maximum message size and do not support manual ping/pong frame control.*

---

## Static Files & SPA Support

Serve directories from the filesystem or embed them directly into the binary at compile time.

```rust
use skyzen::routing::Route;
use skyzen::static_files::{EmbeddedStaticDir, StaticDir};

// 1. Serve from disk (Native only) with SPA fallback:
let disk_routes = Route::new((
    StaticDir::new("/assets", "./public")
        .index_file("index.html")
        .spa(), // Extensionless paths fall back to index.html
));

// 2. Embed files at compile time (Works on both Native and WebAssembly):
static ASSETS: include_dir::Dir = include_dir::include_dir!("$CARGO_MANIFEST_DIR/dist");

let embedded_routes = Route::new((
    EmbeddedStaticDir::new("/", &ASSETS)
        .index_file("index.html")
        .spa(),
));
```

---

## OpenAPI & Documentation

Annotate handlers with `#[skyzen::openapi]` to generate an OpenAPI specification at compile time. Doc comments automatically become endpoint descriptions:

```rust
use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    OpenApi, Result, ToSchema,
};

#[derive(Serialize, ToSchema)]
struct Item {
    id: u64,
    title: String,
}

/// Retrieve an item by unique identifier.
#[skyzen::openapi]
async fn get_item(skyzen::extract::Path(id): skyzen::extract::Path<u64>) -> Result<Json<Item>> {
    Ok(Json(Item {
        id,
        title: format!("Item #{id}"),
    }))
}

fn router() -> Router {
    let routes = Route::new((
        "/items/{id}".get(get_item),
    ));

    // Mount interactive ReDoc documentation at GET /docs
    let redoc_endpoint = routes.openapi().redoc();

    Route::new((
        routes,
        "/docs".get(redoc_endpoint),
    ))
    .build()
}
```

*OpenAPI schema generation is gated to debug builds and native targets, keeping production release binaries and edge WASM bundles lightweight.*

---

## Testing

The `skyzen-test` crate allows testing endpoints and services in-memory without binding TCP sockets or running background servers.

```toml
[dev-dependencies]
skyzen-test = { version = "0.1" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
use serde_json::json;
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    Result,
};
use skyzen_services::Kv;
use skyzen_test::{mock::InMemoryKv, TestContext};

async fn get_name(kv: Kv) -> Result<Json<serde_json::Value>> {
    let name = kv.get_text("user:name").await?.unwrap_or_else(|| "Anonymous".into());
    Ok(Json(json!({ "name": name })))
}

fn app(kv: Kv) -> Router {
    Route::new(("/user".get(get_name),))
        .with(kv)
        .build()
}

#[tokio::test]
async fn test_user_endpoint() {
    // 1. Initialize mock service
    let mock_kv = Kv::new(InMemoryKv::new());
    mock_kv.put("user:name", b"Alice").await.unwrap();

    // 2. Create in-memory test client
    let ctx = TestContext::new();
    let client = ctx.client(app(mock_kv));

    // 3. Send request and assert response
    let response = client.get("/user").send().await;
    response.assert_status_success();
    response.assert_json_path("name", &json!("Alice"));
}
```

See the [Testing Guide](docs/testing-guide.md) for more details on assertion helpers and snapshot testing.

---

## Running on the Edge (Cloudflare Workers)

Skyzen compiles to WebAssembly without changing routing or handler definitions.

### Project Setup

Configure your crate as a `cdylib` in `Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
skyzen = { version = "0.1", default-features = false, features = ["json"] }
```

In `src/lib.rs`:

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};

fn app() -> Router {
    Route::new((
        "/".at(|| async { "Hello from Cloudflare Workers!" }),
    ))
    .build()
}

#[skyzen::main]
fn worker() -> Router {
    app()
}
```

On `wasm32-unknown-unknown`, `#[skyzen::main]` automatically generates the WinterCG `fetch` export.

### Serverless Event Handlers

Cloudflare documents six Worker handlers. Skyzen exports all six: `fetch` from `#[skyzen::main]`,
the Durable Object `alarm` from `Route::on_alarm`, and the four below from dedicated attributes.

Export Queue batch consumers and Cron triggers:

```rust
#[cfg(target_arch = "wasm32")]
#[skyzen::queue]
async fn queue_handler(
    batch: skyzen_cloudflare::CfQueueBatch,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfEventContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    batch.ack_all()?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
#[skyzen::scheduled]
async fn cron_handler(
    event: skyzen_cloudflare::CfScheduledEvent,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfScheduleContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    Ok(())
}
```

Process inbound mail with `#[skyzen::email]` and consume another Worker's logs with
`#[skyzen::tail]`:

```rust
#[cfg(target_arch = "wasm32")]
#[skyzen::email]
async fn email_handler(
    message: skyzen_cloudflare::CfEmailMessage,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfEventContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    // Reject rather than silently drop: the sending server is told why.
    if message.raw_size()? > 10 * 1024 * 1024 {
        message.reject("message too large")?;
        return Ok(());
    }
    message.forward("archive@lexo.cool").await
}

#[cfg(target_arch = "wasm32")]
#[skyzen::tail]
async fn tail_handler(
    traces: Vec<skyzen_cloudflare::TailTraceItem>,
    env: skyzen::runtime::wasm::Env,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    for trace in traces {
        tracing::info!(script = ?trace.script_name, outcome = ?trace.outcome, "traced invocation");
    }
    Ok(())
}
```

### Durable Objects & Object-Scoped SQL

Use `#[skyzen::durable_object]` and `DurableDb` for stateful edge computing backed by SQLite:

```rust
use skyzen::durable::DurableObject;
use skyzen::routing::{CreateRouteNode, Route};
use skyzen::Result;
use skyzen_services::durable::DurableDb;

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[skyzen::durable_object]
struct ChatRoom;

impl DurableObject for ChatRoom {
    fn fetch(&mut self) -> impl skyzen::Endpoint + 'static {
        Route::new((
            "/messages".get(get_messages),
        ))
        .build()
    }
}

async fn get_messages(db: DurableDb) -> Result<skyzen::utils::Json<Vec<String>>> {
    let rows = db.query("SELECT message FROM messages")
        .fetch_all::<String>()
        .await?;
    Ok(skyzen::utils::Json(rows))
}
```

*Skyzen provides `NativeDurableNamespace` on native targets to simulate Durable Objects and SQLite state during local testing.*

See the [Deployment Guide](docs/deployment-guide.md) and [Durable SQL Guide](docs/durable-sql-guide.md).

---

## Skyzen CLI

The `skyzen` CLI automates scaffolding, local emulation, and deployment:

```sh
# Install CLI
cargo install skyzen-cli

# Create a project from a template
skyzen new my-api --template api
skyzen new my-events --template serverless-events
skyzen new my-room --template durable-realtime

# Verify installed platform prerequisites (wrangler, wasm target, etc.)
skyzen doctor

# Run local development with file watching
skyzen dev

# Run local development against Cloudflare Workers emulator
skyzen dev --provider cloudflare

# Deploy to Cloudflare Workers
skyzen deploy --provider cloudflare

# Or to AWS Lambda, or Azure Functions — the same application, no code changes
skyzen deploy --provider aws
skyzen deploy --provider azure
```

---

## Crates Overview

| Crate | Path | Description |
|---|---|---|
| [`skyzen`](.) | Root | Core framework: routing, extractors, responders, middleware, and runtime |
| [`skyzen-core`](core/) | `core/` | Foundational traits (`Extractor`, `Responder`, `Server`), `no_std` compatible |
| [`skyzen-hyper`](hyper/) | `hyper/` | Hyper backend implementing `Server` for native runtimes |
| [`skyzen-lambda`](lambda/) | `lambda/` | AWS Lambda adapter: HTTP invocations and SQS batches (root crate's `lambda` feature) |
| [`skyzen-macros`](macros/) | `macros/` | Procedural macros (`#[skyzen::main]`, `#[skyzen::error]`, `#[skyzen::openapi]`, etc.) |
| [`skyzen-services`](services/) | `services/` | Portable service abstractions (`Kv`, `Storage`, `Queue`, `Db`) |
| [`skyzen-test`](test/) | `test/` | In-memory mocks (`InMemoryKv`, `InMemoryStorage`), test client, and assertion tools |
| [`skyzen-redis`](redis/) | `redis/` | Redis implementation of `KeyValueStore` |
| [`skyzen-s3`](s3/) | `s3/` | S3-compatible implementation of `ObjectStorage` |
| [`skyzen-cloudflare`](cloudflare/) | `cloudflare/` | Cloudflare Workers bindings: KV, R2, Queues, D1, Durable Objects (*wasm32 only*) |
| [`skyzen-aws`](aws/) | `aws/` | AWS implementations: DynamoDB, S3, SQS |
| [`skyzen-azure`](azure/) | `azure/` | Azure implementations: Cosmos DB, Blob Storage, Service Bus |
| [`skyzen-cli`](cli/) | `cli/` | Command-line tool for project creation, local emulation, and deployments |

---

## Guides & Examples

- [Using Portable Services](docs/services-guide.md)
- [SQL Migrations](docs/migrations.md)
- [Testing Guide](docs/testing-guide.md)
- [Deployment Guide](docs/deployment-guide.md)
- [Durable Objects & SQL Guide](docs/durable-sql-guide.md)
- [Skyzen.toml Reference](docs/skyzen-toml-reference.md)
- [Runnable Code Examples](examples/)

---

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

