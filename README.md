# Skyzen

[![crates.io](https://img.shields.io/crates/v/skyzen.svg)](https://crates.io/crates/skyzen)
[![doc.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen)
[![License](https://img.shields.io/crates/l/skyzen.svg)](#license)

Skyzen is an HTTP framework for Rust whose **infrastructure is portable, not just its handlers**.

A handler asks for `Kv`, `Storage`, `Queue` or `Db` and gets a capability, not a vendor SDK. The
same function body runs against Redis and Postgres on a server, Cloudflare KV and D1 at the edge,
DynamoDB and SQS on AWS, Cosmos DB and Blob Storage on Azure — and against in-process fakes in your
tests, with no sockets, no containers and no `wrangler` running.

```rust
use skyzen::{extract::Path, utils::Json, Result};
use skyzen_services::{Db, Kv};

// This signature is the whole point. Nothing here names a provider.
async fn order(Path(id): Path<u64>, kv: Kv, db: Db) -> Result<Json<Order>> {
    if let Some(cached) = kv.get_json::<Order>(&format!("order:{id}")).await? {
        return Ok(Json(cached));
    }
    let order = db
        .query("SELECT id, total FROM orders WHERE id = ?")
        .bind(id)
        .fetch_one::<Order>()
        .await?;
    kv.put_json(&format!("order:{id}"), &order).await?;
    Ok(Json(order))
}
```

---

## Table of Contents

- [Portable Services](#portable-services)
- [Testing Without Infrastructure](#testing-without-infrastructure)
- [One Binary, Four Deployments](#one-binary-four-deployments)
- [Coming From axum](#coming-from-axum)
- [Quick Start](#quick-start)
- [Routing](#routing)
- [Handlers, Extractors & Responders](#handlers-extractors--responders)
- [Error Handling](#error-handling)
- [Middleware & State](#middleware--state)
- [SQL & Migrations](#sql--migrations)
- [WebSockets](#websockets)
- [Static Files & SPA Support](#static-files--spa-support)
- [OpenAPI & Documentation](#openapi--documentation)
- [Skyzen CLI](#skyzen-cli)
- [GitHub Secrets and `.env`](#github-secrets-and-env)
- [Crates Overview](#crates-overview)
- [Guides & Examples](#guides--examples)
- [License](#license)

---

## Portable Services

`skyzen-services` defines four capabilities. Each is a trait a backend implements, a type-erased
wrapper that is both `Middleware` (it injects itself) and `Extractor` (it pulls itself back out),
and a set of convenience methods on top.

| Capability | Wrapper | What the trait covers |
|---|---|---|
| Key–value | `Kv` | `get`/`put`/`put_with_ttl`/`delete`/`exists`, paginated `list`, and the atomics: `put_if_absent`, `compare_and_swap`, `increment`, `expire` |
| Object storage | `Storage` | `get`/`put`/`put_with`/`head`/`delete`, paginated `list`, plus `get_stream`, `put_stream`, `get_range`, `presign_get`, `presign_put` |
| Message queue | `Queue` | produce with `send`/`send_batch`/`send_with`, consume with `receive`/`ack`/`nack` |
| SQL | `Db` | `query(..).bind(..).fetch_one/fetch_all/fetch_optional/fetch_scalar/execute`, typed rows through `#[derive(FromRow)]`, `begin` transactions, `execute_batch`, and `migrate` |

### Provider Matrix

| | In-memory (tests) | Native server | Cloudflare Workers | AWS | Azure |
|---|---|---|---|---|---|
| **`Kv`** | `InMemoryKv` | `Redis` | `CfKv` | `DynamoKv` | `CosmosKv` |
| **`Storage`** | `InMemoryStorage` | `S3Storage` | `CfR2` | `S3Storage` | `AzureBlob` |
| **`Queue`** (produce) | `InMemoryQueue` | `SqsQueue` | `CfQueue` | `SqsQueue` | `ServiceBusQueue`, `AzureStorageQueue` |
| **`Queue`** (consume) | driven by the test | Skyzen polls — `[[native.queue_consumer]]` | the platform pushes | SQS event source mapping | Functions queue trigger |
| **`Db`** | `InMemoryDb` (SQLite) | sqlx — Postgres, MySQL, SQLite | `CfD1` | `RdsDataDb` (Aurora Data API) | `AzureSqlDb` (Azure SQL, dialect `Mssql`) |
| `Db::begin` (transactions) | yes | yes | no — use `execute_batch` | yes | yes |
| `Db::migrate` / `skyzen migrate` | yes | yes | yes (`wrangler d1 migrations`) | in-app runner only | in-app runner only |

Read the columns honestly:

- **Native server** is a *runtime*, not a separate set of backends. Every backend that is a plain
  HTTP client — `SqsQueue`, `DynamoKv`, `S3Storage`, `AzureBlob`, `CosmosKv`, `ServiceBusQueue`,
  `AzureStorageQueue`, `RdsDataDb` — works from a native binary too, and every one of them can be
  declared in [`Skyzen.toml`](docs/skyzen-toml-reference.md)'s `[native.service.*]` /
  `[native.database.*]` wiring rather than constructed by hand. The column names the one this
  table's row is *about*, not the limit of what a native binary can reach.
- **Which `Db` you want on Azure depends on which database it is.** Azure Database for PostgreSQL
  and for MySQL speak the wire protocols sqlx already speaks, so they are an ordinary native
  `[[database]]` and need nothing from `skyzen-azure`. **Azure SQL** speaks T-SQL over TDS, which
  sqlx has no driver for, and is what `AzureSqlDb` exists for — real transactions included. Cosmos
  DB is not SQL here at all: it is wired as a key–value store.
- **Cloudflare has no interactive transactions**, because D1 has none. `Db::begin` returns
  `DbError::TransactionsUnsupported` there; `Db::execute_batch` is D1's atomic unit and is
  supported everywhere.
- A capability a backend genuinely lacks returns `Unsupported` and fails loudly. Nothing silently
  degrades to a racy read-modify-write.

### Wiring a Backend

In code:

```rust
use skyzen_services::{Db, Kv, Storage};

// Native
let kv = Kv::new(skyzen_redis::Redis::connect("redis://127.0.0.1:6379").await?);
let storage = Storage::new(skyzen_s3::S3Storage::from_env("my-bucket").await);
let db = Db::connect_postgres("postgres://localhost/app").await?;
// Azure SQL speaks T-SQL over TDS, which sqlx has no driver for, so it has a backend of its own.
let db = Db::new(skyzen_azure::AzureSqlDb::from_env()?);

// AWS / Azure — plain HTTP clients, so these work from any runtime
let kv = Kv::new(skyzen_aws::DynamoKv::from_env("sessions").await);
let kv = Kv::new(skyzen_azure::CosmosKv::from_env("appdb", "sessions").await?);
let storage = Storage::new(skyzen_azure::AzureBlob::from_env("uploads")?);

// Cloudflare Workers, from the Worker's env bindings
let kv = Kv::new(skyzen_cloudflare::CfKv::from_env(&env, "CACHE")?);
let storage = Storage::new(skyzen_cloudflare::CfR2::from_env(&env, "UPLOADS")?);

// Tests — no external dependency at all
let kv = Kv::new(skyzen_test::mock::InMemoryKv::new());
let memory = skyzen_test::mock::InMemoryDb::with_migrations(&MIGRATIONS).await?;
let db: Db = memory.db().clone();
```

Or declaratively, once, in [`Skyzen.toml`](docs/skyzen-toml-reference.md) — `#[skyzen::main]` reads
it at compile time and generates a named extractor per entry:

```toml
[[service]]
name = "cache"
type = "kv"

[native.service.cache]
backend = "redis"
url_env = "CACHE_URL"

[cloudflare.service.cache]
binding = "CACHE"
```

Any backend goes in that `backend = …` — `dynamodb`, `cosmos`, `blob`, `servicebus`,
`storage-queue`, `rds-data`, `azure-sql` and the rest — each with its own keys, and unknown ones
rejected where they are written. `skyzen add <backend>` installs the crate it needs, and
`skyzen dev` refuses to start when a variable one of them reads is set nowhere.

```rust
// `[[service]] name = "cache"` generates `pub struct Cache(Kv)` with `Deref<Target = Kv>`;
// `[[database]] name = "journal"` generates `JournalDb`. Two KV namespaces are therefore
// ordinary — name the one you mean.
async fn handler(cache: Cache) -> Result<&'static str> {
    cache.put("greeting", b"hello").await?;
    Ok("ok")
}
```

See the [Services Guide](docs/services-guide.md).

---

## Testing Without Infrastructure

Because the capability is the interface, a test swaps the backend rather than the code. `TestClient`
drives the router in process — no TCP socket, no background server, no `wrangler`.

```rust
use serde_json::json;
use skyzen_services::Kv;
use skyzen_test::{mock::InMemoryKv, TestContext};

#[tokio::test]
async fn a_known_user_is_greeted_by_name() {
    let kv = Kv::new(InMemoryKv::new());
    kv.put("user:name", b"Alice").await.unwrap();

    let client = TestContext::new().with_kv(kv.clone()).client(app());

    let response = client.get("/user").send().await;
    response.assert_status_success();
    response.assert_json_path("name", &json!("Alice"));
}
```

`TestContext` has a slot for every capability — `with_kv`, `with_storage`, `with_queue`, `with_db`,
and the Durable Object ones (`with_durable_kv`, `with_durable_db`, `with_alarm`) — so a Workers
application is testable on a native `cargo test`. `#[skyzen::test]` goes further and fills those
slots from your `Skyzen.toml` automatically, optionally applying your migrations first:

```rust
#[skyzen::test(migrations = MIGRATIONS)]
async fn orders_are_persisted(db: Db, client: TestContext) { /* ... */ }
```

See the [Testing Guide](docs/testing-guide.md).

---

## One Binary, Four Deployments

Nothing in an application is annotated for a platform. The native binary reads its environment
before it binds anything, and the `wasm32` build is the Worker:

| Target | How it is selected | Entry point |
|---|---|---|
| Native server | nothing else matched | Tokio + Hyper, `--port` / `--host` / `--listen` |
| AWS Lambda | `AWS_LAMBDA_RUNTIME_API` is set (needs the `lambda` feature) | `lambda_http` for HTTP, partial-batch responses for SQS |
| Azure Functions | `FUNCTIONS_CUSTOMHANDLER_PORT` is set | custom handler; HTTP triggers reach the router, queue triggers reach `#[skyzen::queue]` |
| Cloudflare Workers | compiled to `wasm32-unknown-unknown` | the WinterCG `fetch` export |

One `#[skyzen::queue]` handler is driven four ways: by Skyzen's own polling loop natively, by the
platform on Workers and Lambda, and by the Functions host on Azure. See the
[Deployment Guide](docs/deployment-guide.md).

---

## Coming From axum

Most of what you reach for has a direct equivalent:

| axum | Skyzen | Notes |
|---|---|---|
| `axum::extract::Path<T>` | `skyzen::extract::Path<T>` | Same deserialization into a tuple, struct or primitive; `Params` remains the runtime-keyed escape hatch |
| `axum::extract::Query<T>` | `skyzen::extract::Query<T>` | Backed by `serde_html_form`, so `?tag=a&tag=b` fills a `Vec<String>` — which `serde_urlencoded` cannot |
| `axum::Form<T>` | `skyzen::utils::Form<T>` | Same crate underneath |
| `axum::Json<T>` | `skyzen::utils::Json<T>` | |
| `axum::extract::State<T>` | `skyzen::utils::State<T>` | Attached with `.with(State(value))`; `Route::build()` fails if a handler extracts state no ancestor provides |
| `axum_extra::TypedHeader<H>` | `skyzen::extract::TypedHeader<H>` | The same `headers` crate (`typed-header` feature) |
| `http::HeaderMap`, `Uri`, `Method` | same types, same use | All three implement `Extractor` |
| `axum::middleware::from_fn` | `skyzen::middleware::from_fn` | Closure returns a boxed future |
| `Router::layer` | `Route::layer` / `Router::layer` | Covers the whole router *including* its 404 and 405 paths |
| `Router::fallback` | `Route::fallback` | Plus `Route::method_not_allowed`, which can read `AllowedMethods` |
| `Router::nest` | `"/api".nest(router)` | Mounts an already-built `Router` under a path |
| `tower_http::cors::CorsLayer` | `skyzen::middleware::Cors` | |
| `tower_http::limit::RequestBodyLimitLayer` | `skyzen::middleware::BodyLimit` | On by default at 2 MiB — see below |
| `tower_http::compression` | `skyzen::middleware::CompressionMiddleware` | |
| `tower_http::timeout` | `skyzen::middleware::Timeout` | Native targets only |
| `axum::response::sse` | `skyzen::responder::Sse` | With keep-alives |
| `axum::extract::ws` | `.ws(handler)` on a route | One API over `async-tungstenite` natively and `WebSocketPair` on wasm |

What is genuinely different:

- **No `tower`.** `Middleware` is Skyzen's own trait taking `&self`, so the whole `tower`/
  `tower-http` ecosystem is unavailable. The shipped middleware above is what there is; anything
  else you write yourself, which is ~10 lines.
- **Routing is a tree of values, not a builder chain.** `Route::new((..))` takes a tuple of nodes,
  and `Route::build()` validates the wiring before the first request rather than 500-ing on it.
- **Extraction is `&mut Request`,** not `FromRequest`/`FromRequestParts`. There is one trait, and
  only one extractor per handler may consume the body — a second one is a loud `500` naming both,
  instead of silently seeing an empty body.
- **Handlers cap at 15 arguments.**

Two defaults are safer than axum's, and worth knowing before you port:

- **5xx bodies are redacted.** A `500` returns `"Internal server error"` to the client while the
  full message *and its whole `source()` chain* go to the log. 4xx messages are returned verbatim,
  because they are about the caller's request.
- **Request bodies are capped at 2 MiB** by default, enforced by every buffering extractor, both
  from `Content-Length` and mid-stream for a chunked body. Raise it with `BodyLimit`, or lift it
  with `RequestBodyLimit::disabled()` on a route that streams.

---

## Quick Start

```toml
[dependencies]
skyzen = "0.1"
serde = { version = "1.0", features = ["derive"] }
```

```rust
use serde::{Deserialize, Serialize};
use skyzen::{
    extract::{Path, Query},
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    Result, ToSchema,
};

// A payload carried by `Json`, `Form` or `Query` derives `ToSchema` alongside its serde derive:
// one says how it goes on the wire, the other how the OpenAPI document describes it.
#[derive(Serialize, ToSchema)]
struct MessageResponse {
    message: String,
}

#[derive(Deserialize, ToSchema)]
struct GreetingQuery {
    prefix: Option<String>,
}

async fn health() -> &'static str {
    "OK"
}

async fn greet_user(
    Path(name): Path<String>,
    Query(query): Query<GreetingQuery>,
) -> Result<Json<MessageResponse>> {
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

```sh
cargo run                # binds an open localhost port, or reads PORT / SKYZEN_ADDRESS
cargo run -- --port 8080
```

---

## Routing

Routing trees are built from string path literals through the `CreateRouteNode` trait, and matched
by a radix tree ([`matchit`](https://crates.io/crates/matchit)).

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};

fn router() -> Router {
    Route::new((
        "/".at(home),                     // GET shorthand
        "/posts".get(list_posts).post(create_post),
        "/posts/{id}".put(update_post).patch(patch_post).delete(delete_post),
        "/ws".ws(chat_websocket),         // WebSocket upgrade — native and wasm
    ))
    .build()
}
```

### Path Parameters and Wildcards

`{name}` matches one segment; `{*path}` matches the rest of the path.

`Path<T>` deserializes the captured segments into a tuple, a struct, or a single primitive, so a
malformed segment is a `400` naming it rather than a `.parse()` in every handler. `Params` is the
escape hatch for names only known at runtime.

```rust
use skyzen::extract::Path;
use skyzen::routing::{CreateRouteNode, Params, Route};
use skyzen::Result;

async fn get_user_post(Path((user_id, post_id)): Path<(String, u64)>) -> Result<String> {
    Ok(format!("User {user_id}, Post {post_id}"))
}

async fn serve_asset(params: Params) -> Result<String> {
    Ok(format!("Serving asset: {}", params.get("path")?))
}

let routes = Route::new((
    "/users/{user_id}/posts/{post_id}".get(get_user_post),
    "/assets/{*path}".get(serve_asset),
));
```

### Grouping and Nesting

`.route(..)` groups sub-paths under a node — the child paths are *relative* to it, so a child
repeating the parent's segment registers it twice:

```rust
let api = Route::new((
    "/v1".route((
        "/users".get(list_users).post(create_user),   // /v1/users
        "/users/{id}".get(get_user).delete(delete_user),
    )),
));
```

`"/api".nest(router)` mounts an already-built `Router` under a path, and `Router::routes()` lists
every registered `(method, path)` so a wrongly nested tree is one `dbg!` away.

---

## Handlers, Extractors & Responders

A handler is an `async fn` whose arguments implement `Extractor` and whose return type implements
`Responder`. No macro, no registration.

```rust
use skyzen::{
    extract::{BearerToken, ClientIp, Path},
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

| Extractor | Path | Description |
|---|---|---|
| `Json<T>` | `skyzen::utils::Json` | Deserializes a JSON request body (`T: ToSchema`) |
| `Query<T>` | `skyzen::extract::Query` | Deserializes the query string, repeated keys included (`T: ToSchema`) |
| `Form<T>` | `skyzen::utils::Form` | Deserializes `application/x-www-form-urlencoded` (`T: ToSchema`) |
| `Path<T>` | `skyzen::extract::Path` | Deserializes the captured `{name}` segments into a struct, tuple or primitive |
| `Params` | `skyzen::routing::Params` | Path parameters by name at runtime (`params.get("id")?`) |
| `Multipart` | `skyzen::utils::Multipart` | Streams multipart form data and file uploads |
| `State<T>` | `skyzen::utils::State` | Shared state attached with `.with(State(..))` |
| `CookieJar` | `skyzen::utils::CookieJar` | Reads request cookies; also a `Responder` for setting them |
| `BearerToken` | `skyzen::extract::BearerToken` | The bearer token from `Authorization` |
| `ClientIp` | `skyzen::extract::ClientIp` | Client IP, honouring `X-Forwarded-For` and `CF-Connecting-IP` |
| `TypedHeader<H>` | `skyzen::extract::TypedHeader` | One RFC-typed header via `headers` (`typed-header` feature) |
| `Kv`, `Storage`, `Queue`, `Db` | `skyzen_services::*` | Portable services injected into the request |
| `String`, `Bytes`, `ByteStr`, `Body` | `skyzen::http_kit::*` | The request body, buffered or streamed |
| `HeaderMap`, `Uri`, `Method` | `skyzen::http_kit::*` | Request metadata |
| `RequestBodyLimit` | `skyzen::RequestBodyLimit` | The body cap in force, so a handler can size its own reads |
| `Option<T>`, `Result<T, _>` | — | Wrap any extractor to make its failure recoverable |

A body payload carries `T: ToSchema` so that what the endpoint documents is what it actually
serializes — the bound is what makes the generated document trustworthy rather than best-effort.
`#[derive(ToSchema)]` beside the serde derive is the whole cost, and every primitive, collection
and `serde_json::Value` already has one. The derive expands to `::utoipa::…` paths, so an
application declares `utoipa = "5"` alongside `skyzen` (`skyzen new` does this for you);
`skyzen::ToSchema` re-exports the trait the bound is written against.

`Path<T>` is the exception and takes no bound: the route pattern names its parameters, and
`Path<(String, u32)>` for a multi-segment route has no schema to give. `#[skyzen::openapi]` types
those parameters by probing the payload at the handler's own call site instead.

### Built-in Responders

| Responder | Description |
|---|---|
| `&'static str`, `String`, `Bytes` | Plain text or raw bytes |
| `Json<T>` / `PrettyJson<T>` | Serializes `T` to `application/json` (`T: ToSchema`) |
| `Html<T>` | Sends its payload as `text/html; charset=utf-8` |
| `StatusCode` | An empty response with that status |
| `Redirect` | `to` (302), `see_other` (303), `temporary` (307), `permanent` (308), or `with_status` |
| `HeaderMap`, `(HeaderName, HeaderValue)` | Sets response headers |
| `Sse` | Streams Server-Sent Events, with keep-alives |
| `CookieJar` | Emits the `Set-Cookie` headers it accumulated |
| `(StatusCode, T)`, `(HeaderMap, T)`, tuples | Compose an explicit status or headers with any responder |
| `Result<T, E>` | `T` on `Ok`; `E: HttpError` becomes an HTTP error response |

---

## Error Handling

`#[skyzen::error]` implements `Display`, `std::error::Error` (including `source()` for
`#[from]`/`#[source]` fields) and `HttpError`, mapping each variant to a status:

```rust
use skyzen::StatusCode;

#[skyzen::error]
pub enum AppError {
    #[error("item with id {0} was not found", status = NOT_FOUND)]
    NotFound(u64),

    #[error("validation error on field '{field}': {reason}", status = BAD_REQUEST)]
    Validation { field: &'static str, reason: String },

    #[error("unauthorized access", status = StatusCode::UNAUTHORIZED)]
    Unauthorized,

    #[error("upstream service timeout", status = GATEWAY_TIMEOUT)]
    Timeout,
}
```

### Mixing Error Types with `skyzen::Result`

A handler that fails in more than one way returns `skyzen::Result<T>`. Anything implementing
`HttpError` — a route-parameter rejection, a `Json` rejection, a `KvError`, your own
`#[skyzen::error]` enum — converts with `?` **and keeps its own status**:

```rust
use skyzen::{extract::Path, Result};
use skyzen_services::Kv;

async fn read_profile(Path(id): Path<u64>, kv: Kv) -> Result<String> {
    let raw = kv.get_text(&format!("profile:{id}")).await?;   // 500 if the store is unreachable
    raw.ok_or_else(|| {
        skyzen::Error::msg("no such profile").set_status(skyzen::StatusCode::NOT_FOUND)
    })
}
```

Errors with no HTTP meaning of their own do **not** convert implicitly — guessing a status is how a
client error becomes a 500. State one:

```rust
use skyzen::{Context, Result, ResultExt, StatusCode};

async fn read_api_key() -> Result<String> {
    // `.status(..)` states the status; `.context(..)` adds a breadcrumb and keeps it.
    std::env::var("UPSTREAM_API_KEY")
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .context("reading the upstream API key")
}
```

`ResultExt::status_msg`, and `Option::status`/`status_msg`, cover the case with no error value.

### What the Client Sees

- **4xx** — the formatted message, as JSON: `404 {"error":"item with id 42 was not found"}`.
- **5xx** — `"Internal server error"`, so a database or system detail cannot leak, while the full
  message and its `source()` chain are logged server-side.

---

## Middleware & State

```rust
use std::sync::Arc;
use skyzen::{routing::{CreateRouteNode, Route, Router}, utils::State};

#[derive(Clone)]
struct AppConfig { api_key: String }

async fn handler(State(config): State<Arc<AppConfig>>) -> String {
    format!("key length {}", config.api_key.len())
}

fn router() -> Router {
    let config = Arc::new(AppConfig { api_key: "secret".into() });
    Route::new(("/info".at(handler),)).with(State(config)).build()
}
```

### Writing Middleware

`Middleware` takes **`&self`** and is stored once as `Arc<dyn MiddlewareObj>`, never cloned per
request — so state kept in an atomic or a channel really persists:

```rust
use std::sync::atomic::{AtomicUsize, Ordering};
use skyzen::{middleware::{Middleware, Next}, Error, Request, Response};

#[derive(Debug, Default)]
struct CountRequests { seen: AtomicUsize }

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

### Attachment Scopes

| Call | Covers |
|---|---|
| `RouteNode::with(m)` | one path node's endpoints |
| `Route::with(m)` / `Route::middleware(m)` | every endpoint in the subtree |
| `Route::layer(m)` / `Router::layer(m)` | the entire router, **including its 404 and 405 responses** |

CORS, tracing and request-id middleware belong on `layer`: a preflight `OPTIONS` arrives at a path
whose registered methods are `GET`/`POST`, so it has to be answered before the router synthesizes a
405.

### Shipped Middleware

| Middleware | Purpose |
|---|---|
| `Cors` | Answers preflights and decorates cross-origin responses; rejects credentials + wildcard origin at construction |
| `CompressionMiddleware` | gzip/deflate negotiation, skipping HEAD and unknown-length streams |
| `BodyLimit` | Sets the request body cap for the routes it covers (2 MiB applies with no middleware at all) |
| `Timeout` | Abandons a request that outruns its budget with `408` (native targets only) |
| `ErrorHandlingMiddleware` | Renders endpoint errors into responses |
| `AuthMiddleware` | Authenticates the request and injects `AuthUser<U>` |
| `State<T>`, `Kv`, `Storage`, `Queue`, `Db` | Inject a value the matching extractor reads back |

### Custom 404 and 405 Responses

```rust
use skyzen::{routing::{AllowedMethods, CreateRouteNode, Route}, Result, Uri};

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

Both run inside the router's layers.

### Wiring Checked at Build Time

`Route::build()` walks the tree and fails if a handler extracts a `State<T>` or `AuthUser<U>` that
no middleware on its route provides, naming the path and the call that would fix it — instead of
returning a 500 on the first request that reaches the endpoint. `Route::try_build()` returns the
`RouteBuildError` rather than panicking.

---

## SQL & Migrations

Schema lives in plain `.sql` files, embedded at compile time and applied by a runner that works on
every backend `Db` works on:

```rust
use skyzen::embed_migrations;
use skyzen_services::Migrations;

static MIGRATIONS: Migrations = embed_migrations!("migrations");

let report = db.migrate(&MIGRATIONS).await?;
```

The same files are what `skyzen migrate` applies from the CLI, and what
`#[skyzen::test(migrations = MIGRATIONS)]` applies to a test database. Each applied file's checksum
is recorded, so an edited migration is refused rather than skipped. See the
[Migrations Guide](docs/migrations.md).

### Typed Rows

Binding is typed — `DbValue` carries timestamps, UUIDs, exact decimals and JSON documents — and
reading is typed the same way. `#[derive(FromRow)]` reads one column per field, through the field's
own type:

```rust
use skyzen::{Column, FromRow};

/// A newtype and a state machine, each stored in one column, in both directions.
#[derive(Column)]
struct CustomerId(Uuid);

#[derive(Column)]                    // "awaiting_payment", "shipped", "cancelled"
enum OrderState { AwaitingPayment, Shipped, Cancelled }

#[derive(FromRow)]
struct Order {
    id: Uuid,
    #[row(rename = "customer")]
    customer_id: CustomerId,
    state: OrderState,
    placed_at: DateTime<Utc>,
    total: BigDecimal,
    budget: u64,
    #[row(json)]
    items: Vec<LineItem>,
}

let order: Order = db.query("SELECT * FROM orders WHERE id = ?").bind(id).fetch_one().await?;
```

A `Uuid` column is a string on PostgreSQL and sixteen bytes on SQLite; a `NUMERIC` is a string
everywhere. The field's type is what decides how the column is read, so the same struct decodes on
every backend. A missing column, or one holding something the field's type does not accept, is an
error naming both — never a default.

Single-column queries need no struct at all:

```rust
let orders: i64 = db.query("SELECT COUNT(*) FROM orders").fetch_scalar().await?;
let ids: Vec<CustomerId> = db.query("SELECT customer FROM orders").fetch_scalars().await?;
```

`OrderState::TOKENS` is the list of tokens the enum stores, so a `CHECK (state IN (…))` constraint
can be checked against the type instead of drifting from it. For a type whose `Deserialize` someone
else wrote, `JsonRow<T>` hands the whole row to `serde`.

Durable Objects get their own object-scoped SQLite through `DurableDb`:

```rust
use skyzen::durable::DurableObject;
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::{utils::Json, Result};
use skyzen_services::durable::DurableDb;

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[skyzen::durable_object]
struct ChatRoom;

impl DurableObject for ChatRoom {
    fn fetch(&mut self) -> Router {
        Route::new(("/messages".get(get_messages),)).build()
    }
}

#[derive(skyzen::FromRow, serde::Serialize)]
struct Message { message: String }

async fn get_messages(db: DurableDb) -> Result<Json<Vec<Message>>> {
    // One column per field, decoded by the field's own type.
    Ok(Json(db.query("SELECT message FROM messages").fetch_all::<Message>().await?))
}
```

`NativeDurableNamespace` simulates Durable Objects and their SQLite state on native targets, so the
same code is testable without wasm. See the [Durable Object + SQL Guide](docs/durable-sql-guide.md).

---

## WebSockets

One API compiling to `async-tungstenite` natively and `WebSocketPair` on wasm:

```rust
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use skyzen::routing::{CreateRouteNode, Route, Router};

#[derive(Serialize, Deserialize)]
struct ChatPayload { user: String, text: String }

fn router() -> Router {
    Route::new((
        "/ws".ws(|mut socket| async move {
            // `next()` yields `None` on a clean close and `Some(Err(..))` on a transport failure,
            // so `?` keeps the two apart.
            while let Some(message) = socket.next().await {
                if let Some(text) = message?.into_text() {
                    socket.send_text(format!("Echo: {text}")).await?;
                }
            }
            Ok::<_, skyzen::Error>(())
        }),
        "/ws/json".ws(|mut socket| async move {
            while let Some(chat) = socket.recv_json::<ChatPayload>().await {
                let chat = chat?;
                socket
                    .send(&ChatPayload {
                        user: "server".into(),
                        text: format!("Hello, {}", chat.user),
                    })
                    .await?;
            }
            Ok::<_, skyzen::Error>(())
        }),
    ))
    .build()
}
```

A session handler may return `()` or a `Result`, and returning a `Result` is what makes `?` usable:
a failed send is logged and closes the connection with `websocket::INTERNAL_ERROR` rather than
being discarded.

*WASM WebSockets enforce a 1 MiB maximum message size and have no manual ping/pong control.*

---

## Static Files & SPA Support

```rust
use skyzen::routing::Route;
use skyzen::static_files::{EmbeddedStaticDir, StaticDir};

// From disk (native only). Streamed, with ETag, Last-Modified, Range and 304/206 handling.
let disk_routes = Route::new((
    StaticDir::new("/assets", "./public")
        .index_file("index.html")
        .spa(), // extensionless paths fall back to index.html
));

// Embedded at compile time — works on native and wasm alike.
static ASSETS: include_dir::Dir = include_dir::include_dir!("$CARGO_MANIFEST_DIR/dist");

let embedded_routes = Route::new((
    EmbeddedStaticDir::new("/", &ASSETS).index_file("index.html").spa(),
));
```

---

## OpenAPI & Documentation

`#[skyzen::openapi]` builds the specification at compile time from the handler's extractors,
responders and doc comments:

```rust
use serde::Serialize;
use skyzen::{
    extract::Path,
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    Result, ToSchema,
};

#[derive(Serialize, ToSchema)]
struct Item { id: u64, title: String }

/// Retrieve an item by unique identifier.
#[skyzen::openapi]
async fn get_item(Path(id): Path<u64>) -> Result<Json<Item>> {
    Ok(Json(Item { id, title: format!("Item #{id}") }))
}

fn router() -> Router {
    let routes = Route::new(("/items/{id}".get(get_item),));
    let redoc = routes.openapi().redoc();     // interactive ReDoc page
    Route::new((routes, "/docs".get(redoc))).build()
}
```

*Schema generation is gated to debug builds and native targets, so release binaries and edge wasm
bundles stay small.*

---

## Skyzen CLI

```sh
cargo install skyzen-cli
```

The generator and the wasm optimizer are linked into the binary, so that one install is the whole
toolchain — there is no `wasm-bindgen` or `wasm-opt` to add.

```sh
skyzen new my-api --template api      # also: minimal, serverless-events, durable-realtime
skyzen add redis postgres             # `cargo add` the crates a capability needs
skyzen doctor                         # toolchain, wasm target, manifest, bindings, auth
skyzen dev                            # native, restarts on change
skyzen dev --provider cloudflare      # workerd, rebuilding the wasm on change
skyzen build --provider cloudflare    # artifacts only; prints raw and gzipped wasm size
skyzen provision                      # create the KV/D1 resources the manifest has no id for
skyzen migrate                        # apply pending SQL migrations
skyzen migrate status                 # what is applied, what is pending
skyzen deploy --provider cloudflare   # or aws, or azure — same application, no code changes
skyzen logs --env staging             # stream the deployed application's logs
skyzen secret set API_KEY             # value read from stdin
skyzen completions fish
```

`--env <name>` selects a `[cloudflare.env.<name>]` overlay, `--dry-run` prints the plan without
running it, and `--manifest` points at a `Skyzen.toml` elsewhere.

---

## GitHub Secrets and `.env`

Values that must not live in git go in `.env` locally and in GitHub Actions secrets in CI. The CLI
reads them when it loads `Skyzen.toml`; `#[skyzen::main]` does not, so `cargo build` never depends
on a secret the compiler does not have.

There are three different environments. They are not interchangeable:

| Where | What it is |
|---|---|
| `url_env = "CACHE_URL"` | The **name** of a variable the native process reads after it starts |
| `id = "${CACHE_NAMESPACE_ID}"` | A **deploy-time** placeholder. The CLI expands it before generating `wrangler.toml` |
| `skyzen secret set API_KEY` | A **Worker secret**. `[cloudflare.vars]` is plaintext and committed; do not put credentials there |

### Local

`skyzen new` writes `.env.example` from the manifest and gitignores `.env`, `.env.local`, and
wrangler's `.dev.vars`. Copy the example, fill it in, and run:

```sh
cp .env.example .env
skyzen dev
```

`.env.local` overrides `.env`. A variable already in the process environment wins over both.
`skyzen dev` refuses to start when a name the native wiring declared is set nowhere.

### `Skyzen.toml`

String values may contain `${NAME}` placeholders. The CLI expands them from the process
environment, then from `.env` / `.env.local`, when it reads the file — including `skyzen deploy`,
`skyzen provision`, and `skyzen doctor`. A missing name fails the load, naming the TOML path.

```toml
[cloudflare]
account_id = "${CLOUDFLARE_ACCOUNT_ID}"

[[cloudflare.kv_namespaces]]
binding = "CACHE"
id = "${CACHE_NAMESPACE_ID}"
```

`CLOUDFLARE_API_TOKEN` stays out of the file: wrangler reads it from the environment (and from
`.env`) on its own. Do not wrap `url_env`, `binding`, or service names in `${}` — those are
consumed at compile time as literal names.

### GitHub Actions

Skyzen does not need a workflow that maps every secret into `env:`. Keep the same file you use
locally in **one** repository secret, write it onto the runner, then deploy:

```yaml
- run: printf '%s\n' "$DEPLOY_ENV" > .env
  env:
    DEPLOY_ENV: ${{ secrets.DEPLOY_ENV }}
- run: skyzen deploy --provider cloudflare
```

`DEPLOY_ENV` is the body of `.env`, not a single key. The CLI interpolates from that file; wrangler
picks up `CLOUDFLARE_API_TOKEN` from it the same way. If the job already exports variables, those
win without a `.env`.

### What the CLI refuses

A documented credential form stored as a literal in `Skyzen.toml` (a GitHub PAT, a PEM private key,
a URL with a password, an `AKIA…` access key id) fails the load. So does a git checkout that is
**tracking** `.env`, `.env.local`, or `.dev.vars`. A `vars` / `[aws.env]` key whose *name* looks
like a secret but whose value is not a known form is a warning, not a hard failure. Errors name
the path and the kind; they never print the value.

The syntax and the merge rules are in the [Skyzen.toml reference](docs/skyzen-toml-reference.md#deploy-time-interpolation).

---

## Crates Overview

| Crate | Path | Description |
|---|---|---|
| [`skyzen`](.) | Root | Routing, extractors, responders, middleware, static files, WebSockets, runtime |
| [`skyzen-core`](core/) | `core/` | `Extractor`, `Responder`, `Middleware`, `Server`, the error types; `no_std`-capable |
| [`skyzen-macros`](macros/) | `macros/` | `#[skyzen::main]`, `#[skyzen::error]`, `#[skyzen::openapi]`, `#[skyzen::queue]`, `#[skyzen::test]`, `embed_migrations!`, … |
| [`skyzen-manifest`](manifest/) | `manifest/` | The one typed `Skyzen.toml` schema, shared by the macros and the CLI |
| [`skyzen-services`](services/) | `services/` | The portable capabilities: `Kv`, `Storage`, `Queue`, `Db`, migrations, durable variants |
| [`skyzen-test`](test/) | `test/` | `TestClient`, `TestContext`, in-memory backends, assertions, `insta` snapshots |
| [`skyzen-hyper`](hyper/) | `hyper/` | The Hyper `Server` implementation for native runtimes |
| [`skyzen-lambda`](lambda/) | `lambda/` | AWS Lambda adapter — HTTP invocations and SQS batches (root crate's `lambda` feature) |
| [`skyzen-redis`](redis/) | `redis/` | Redis `KeyValueStore` |
| [`skyzen-s3`](s3/) | `s3/` | S3-compatible `ObjectStorage` |
| [`skyzen-cloudflare`](cloudflare/) | `cloudflare/` | Workers KV, R2, Queues, D1, Durable Objects, secrets store, `request.cf` (*wasm32 only*) |
| [`skyzen-cloudflare-admin`](cloudflare-admin/) | `cloudflare-admin/` | Cloudflare REST client, used by `skyzen provision` |
| [`skyzen-aws`](aws/) | `aws/` | `DynamoKv`, `SqsQueue`, `RdsDataDb`, and `S3Storage` re-exported |
| [`skyzen-azure`](azure/) | `azure/` | `CosmosKv`, `AzureBlob`, `ServiceBusQueue`, `AzureStorageQueue`, `AzureSqlDb` |
| [`skyzen-cli`](cli/) | `cli/` | The `skyzen` binary: scaffolding, local emulation, provisioning, migrations, deployment |

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
