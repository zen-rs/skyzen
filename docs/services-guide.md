# Using Portable Services

Skyzen's portable capability layer lets you write business logic against `Kv`, `Storage`, `Queue`, and `Db` instead of provider SDK types. Native and Cloudflare automatic wiring are built in today; provider-specific extensions remain available when you intentionally need more than the portable minimum.

## Mental Model

Every Skyzen service follows a three-layer architecture:

```
Trait (KeyValueStore)       ← Implement this for your backend
    ↓
Wrapper (Kv)                ← Type-erased, object-safe, Clone
    ↓
Extractor (handler arg)     ← Pulled from request extensions automatically
```

1. **Trait** — A public, ergonomic trait (e.g. `KeyValueStore`) that backend crates implement. Returns `impl Future`, so it's *not* object-safe but easy to write.
2. **Wrapper** — A struct (e.g. `Kv`) that holds `Box<dyn KeyValueStoreObj>`. An internal bridge trait converts any `KeyValueStore` into the object-safe form automatically.
3. **Extractor** — The wrapper implements `Extractor`, so handlers receive it as a function argument. No manual lookup required.

## Service Traits

| Trait | Wrapper | Operations |
|-------|---------|------------|
| `KeyValueStore` | `Kv` | `get`, `put`, `delete`, `list` + `get_json`, `get_text`, `put_json` |
| `ObjectStorage` | `Storage` | `get`, `put`, `delete`, `list`, `head` |
| `MessageQueue` | `Queue` | `send`, `send_batch` + `send_json`, `send_json_batch` |
| `DbBackend` | `Db` | `query(...).bind(...).fetch_*`, `execute` |

Cloudflare-specific primitives such as `CfD1`, `DurableDb`, and Durable Objects are provider extensions, not part of the portable core.

## Writing a Handler

Handlers declare service wrappers as arguments — Skyzen extracts them from request extensions:

```rust
use skyzen_services::{Kv, Storage};
use skyzen::utils::Json;

async fn upload(kv: Kv, storage: Storage, Json(body): Json<UploadRequest>) -> Result<&'static str> {
    // Store metadata in KV
    kv.put_json(&format!("file:{}", body.name), &body.metadata).await?;

    // Store file bytes in object storage
    storage.put(&body.name, body.data).await?;

    Ok("uploaded")
}
```

This handler works identically whether `Kv` is backed by Redis, DynamoDB, Cloudflare KV, or an in-memory mock.

## Wiring Services Manually

Before a handler can extract `Kv` or `Storage`, you must inject the concrete implementation as middleware. Here's how to wire Redis + S3 on native:

```rust
use skyzen::routing::{CreateRouteNode, Route};
use skyzen_redis::Redis;
use skyzen_s3::S3Storage;
use skyzen_services::{Kv, Storage};

#[skyzen::main]
async fn main() -> Router {
    // Create concrete backends
    let redis = Redis::connect("redis://127.0.0.1:6379").await.unwrap();
    let s3 = S3Storage::from_env("my-bucket");

    // Wrap in type-erased service wrappers
    let kv = Kv::new(redis);
    let storage = Storage::new(s3);

    Route::new((
        "/upload".post(upload),
    ))
    .with(kv)        // Injects Kv into all request extensions
    .with(storage)   // Injects Storage into all request extensions
    .build()
}
```

## Wiring via `Skyzen.toml`

For projects using `#[skyzen::main]`, declare logical capabilities once and provide target-specific wiring:

```toml
[[service]]
name = "cache"
type = "kv"

[[service]]
name = "uploads"
type = "storage"

[[database]]
name = "main"
type = "sql"

[native.service.cache]
backend = "redis"
url_env = "CACHE_URL"

[native.service.uploads]
backend = "s3"
bucket_env = "UPLOADS_BUCKET"

[native.database.main]
backend = "postgres"
url_env = "DATABASE_URL"

[cloudflare.service.cache]
binding = "CACHE"

[cloudflare.service.uploads]
binding = "UPLOADS"

[cloudflare.database.main]
binding = "DB"
```

`#[skyzen::main]` reads these declarations and injects the portable wrappers automatically. Provider SDK types stay in generated wiring; handlers keep using `Kv`, `Storage`, and `Db`.

## Platform Switching

The same handler code runs against the same wrapper types. Only the wiring changes:

### Native (Redis + S3)

```rust
let kv = Kv::new(Redis::connect("redis://localhost:6379").await?);
let storage = Storage::new(S3Storage::from_env("my-bucket"));
```

### Cloudflare Workers (KV + R2)

On WASM targets, services are created from the Workers environment bindings inside the request handler or startup:

```rust
let kv = Kv::new(CfKv::from_env(&env, "MY_KV")?);
let storage = Storage::new(CfR2::from_env(&env, "MY_R2")?);
```

### Testing (In-Memory)

```rust
let kv = Kv::new(InMemoryKv::new());
let storage = Storage::new(InMemoryStorage::new());
```

Notice the handler function (`upload`) never changes. Only the one-line construction of each backend differs.

Provider crates for AWS and Azure are still available, but they are infrastructure-layer backends today. Runtime parity for those targets is not yet part of the portable core.

## Platform Implementations

| Service | Native | Cloudflare | AWS | Azure | Test |
|---------|--------|------------|-----|-------|------|
| Key-Value | [`skyzen-redis`](../redis/) | `CfKv` | `DynamoKv` | `CosmosKv` | `InMemoryKv` |
| Object Storage | [`skyzen-s3`](../s3/) | `CfR2` | `S3Storage` | `AzureBlob` | `InMemoryStorage` |
| Message Queue | — | `CfQueue` | `SqsQueue` | `ServiceBusQueue` | `InMemoryQueue` |
| Portable SQL | `Db` via sqlx | `Db` via D1 | planned wiring | planned wiring | — |

## Service Futures Are `Send`, Including On WASM

Every service trait bounds its futures with a plain `Send`, on every target:

```rust
pub trait KeyValueStore: Send + Sync + Clone + 'static {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, KvError>> + Send;
}
```

There is no conditional relaxation on `wasm32`, and no alias hiding one. The wrappers (`Kv`,
`Storage`, `Queue`, `Db`, …) are handed to handlers through `http::Extensions`, whose entries are
required to be `Send + Sync` regardless of target, so a `!Send` service future could not be stored
there anyway.

WASM backends satisfy the bound at the JS boundary instead of in the trait. Each backend struct in
`skyzen-cloudflare` holds its JS handle in a private field and gets `Clone`/`Debug` plus a
contained `unsafe impl Send + Sync` from one macro:

```rust
pub struct CfKv {
    ns: ffi::KvNamespace,
}

// Emits Clone, Debug, and:
//   SAFETY: Workers WASM executes on a single thread; JS handles are safe
//   to mark Send/Sync.
//   unsafe impl Send for CfKv {}
//   unsafe impl Sync for CfKv {}
impl_js_handle_traits!(CfKv { ns });
```

Awaited promises go through `worker::send::IntoSendFuture` (`JsFuture::from(promise).into_send()`),
which re-labels a `!Send` JS future as `Send` under the same single-threaded argument. Writing a new
WASM backend means following that recipe — not weakening the trait.

## Database Access

Portable SQL uses `Db`:

```rust
use skyzen_services::Db;

let users = db
    .query("SELECT id, name FROM users WHERE active = ?")
    .bind(true)
    .fetch_all::<User>()
    .await?;
```

Enable the required runtime and database features:

```toml
[dependencies]
skyzen-services = { version = "0.1", features = ["runtime-tokio-rustls", "postgres"] }
```

Portable SQL is intentionally the minimum common surface. When you need provider-specific features such as Durable Object local SQLite or D1-specific metadata, drop down to the provider APIs explicitly.

For object-scoped SQL that runs on both native and Cloudflare, continue with the [Durable Object + SQL Guide](durable-sql-guide.md).
