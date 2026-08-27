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
| `KeyValueStore` | `Kv` | `get`, `put`, `put_with_ttl`, `delete`, `exists`, `list` + the atomics below + `get_json`, `get_text`, `put_json`, `list_all` |
| `ObjectStorage` | `Storage` | `get`, `put`, `put_with`, `delete`, `list`, `head`, `get_stream`, `put_stream`, `get_range`, `presign_get`, `presign_put` |
| `MessageQueue` | `Queue` | `send`, `send_batch`, `send_with`, `receive`, `ack`, `nack` + `send_json`, `send_json_batch`, `receive_json` |
| `DbBackend` | `Db` | `query(...).bind(...).fetch_*`, `execute` |

Cloudflare-specific primitives such as `CfD1`, `DurableDb`, and Durable Objects are provider extensions, not part of the portable core.

### Atomic KV Primitives

A KV store is only usable for locks, idempotency keys and rate limiters if it can write
conditionally, so `KeyValueStore` also carries:

| Method | Returns | Use |
|--------|---------|-----|
| `put_if_absent(key, value)` | `true` when written | distributed lock, idempotency key |
| `compare_and_swap(key, expected, new)` | `true` when swapped | optimistic concurrency |
| `increment(key, delta)` | the new counter value | rate limits, counters |
| `expire(key, ttl)` | `false` when the key is absent | sliding-expiration sessions |

`compare_and_swap` reports a lost race as `Ok(false)`, not as an error — losing an optimistic
update is an ordinary outcome, and `KvError::Conflict` is reserved for conflicts a backend raises
on its own. Each of these has a `KvError::Unsupported` default, so a backend without the primitive
(Cloudflare KV has no conditional write) fails loudly instead of degrading to a racy
read-modify-write.

### Listing

`list` takes `KvListOptions { prefix, limit, cursor }` and returns `KvListResult { keys, cursor }`,
mirroring `ObjectStorage::list`. Pagination is the platform's own — Redis `SCAN`, `DynamoDB`'s
`ExclusiveStartKey`, Cloudflare KV's list cursor — so a large namespace is never materialized at
once:

```rust
let page = kv
    .list(KvListOptions::new().with_prefix("session:").with_limit(100))
    .await?;
for key in page.keys { /* ... */ }
// `page.cursor` is `Some(..)` while more keys remain.
```

`limit` is a target, not a hard cap: cursors are positional, so a backend that has already read
past it cannot drop the surplus without those keys being skipped on resume. Overshoot is bounded
by one native page (Redis' `SCAN COUNT`, one `DynamoDB` scan page); Cloudflare KV and
`InMemoryKv` honour the limit exactly.

`Kv::list_all(prefix)` drains every page for you. It is documented as potentially expensive: it
holds the whole key set in memory and, on `DynamoDB`, is a full table scan.

### Moving Large Objects

`get`/`put` move an object as one `Vec<u8>`, which is a hard ceiling on an edge runtime: a
Cloudflare Worker has roughly 128 MB of memory, so a video or a database dump cannot round-trip
through them. Three additions avoid materializing the body:

| Method | Use |
|--------|-----|
| `get_stream(key)` / `put_stream(key, stream, content_length, options)` | chunked transfer through a `StorageStream` (a boxed `Stream<Item = Result<Bytes, StorageError>>`) |
| `get_range(key, ByteRange)` | answering an HTTP `Range` request without downloading the whole object |
| `presign_get(key, expires_in)` / `presign_put(key, expires_in, options)` | handing the client a `PresignedRequest` so the transfer never touches your server |

`ByteRange` mirrors the two forms an HTTP `Range` header can take — `FromStart { offset, length }`
and `Suffix(n)` — and `ByteRange::resolve(total)` clamps one against a known object size, returning
`None` for a range that selects nothing. A ranged `StorageObject` carries only the requested slice
in `body` while `metadata.size` still reports the whole object, which is exactly the pair a
`Content-Range` header needs.

All five have `StorageError::Unsupported` defaults this wave; provider crates implement them
natively as each platform's wave lands. `InMemoryStorage` implements all of them, presigning a
deterministic `memory://` URL that is documented as **not fetchable**.

### Producing and Consuming Queue Messages

Producers get `send`, `send_batch`, and `send_with(message, SendOptions::new().with_delay(d))` for
scheduled work (SQS `DelaySeconds`, Cloudflare Queues `delaySeconds`).

Consumption comes in two shapes, and `MessageQueue` covers both:

- **Push** — the platform invokes your worker with a batch. Cloudflare Queues works this way;
  Skyzen surfaces it through `#[skyzen::queue]`, `QueueBatch` and `QueueBatchDisposition`.
- **Pull** — the consumer asks for messages and settles them itself. SQS and Azure Service Bus work
  this way:

```rust
let messages = queue
    .receive_json::<Job>(ReceiveOptions::new().with_max_messages(10))
    .await?;

for message in messages {
    match run(&message.body).await {
        Ok(()) => queue.ack(&message.receipt).await?,
        Err(_) => queue.nack(&message.receipt, QueueRetry::new().with_delay_seconds(30)).await?,
    }
}
```

`ReceivedMessage::receipt` is the provider's own settle token (an SQS receipt handle, a Service Bus
lock token) wrapped in an opaque `MessageReceipt`; `attempts` counts deliveries, so a value above
1 means an earlier delivery was never acknowledged. A message left unsettled returns to the queue
when its visibility timeout lapses. A push-only backend leaves `receive`/`ack`/`nack` at their
`QueueError::Unsupported` defaults rather than returning a silent empty batch.

### Consuming a Queue Natively

Writing that pull loop by hand is not the point, though — `#[skyzen::queue]` is dual-target, and
declaring a consumer in `Skyzen.toml` is what makes Skyzen run the loop for you:

```toml
[[service]]
name = "jobs"
type = "queue"

[native.service.jobs]
backend = "sqs"
url_env = "JOBS_QUEUE_URL"

[[native.queue_consumer]]
service = "jobs"
concurrency = 4
batch_size = 10
poll_wait = "20s"
retry_delay = "30s"
```

```rust
#[skyzen::queue]
async fn queue(batch: QueueBatch<Job>) -> QueueBatchDisposition {
    // Exactly the handler a Cloudflare Worker runs, invoked here by Skyzen's own consumer loop.
    QueueBatchDisposition::ack_all()
}
```

`#[skyzen::main]` starts one loop per `concurrency` slot beside the HTTP server, on the same
executor and the same service instance the `Jobs` extractor injects into handlers. Each loop
receives a batch, hands it to the annotated function, and settles every message with what the
function returned: `()` and `Ok(())` acknowledge the batch, an `Err` or a panic retries it, and a
`QueueBatchDisposition` settles message by message. A retry with no delay of its own uses the
manifest's `retry_delay`. `cargo run -p skyzen-example-queue-consumer` is a working one.

What to expect of it:

- **At-least-once.** A batch is settled only after the handler returns, so a process that dies
  mid-batch leaves its messages leased and the visibility timeout redelivers them. Handlers must
  be idempotent.
- **Ordering is not preserved**, least of all across concurrency slots.
- **Backends cap the batch** (SQS at 10, Azure Storage at 32) and the loop always long-polls for
  `poll_wait` — which is why Service Bus, whose receive requires an explicit wait, just works, and
  why Azure Storage queues emulate one by re-polling for the same interval. A backend that answers
  an empty receive early leaves the loop idle for the remainder instead of spinning.
- **A backend that cannot pull at all** — a Cloudflare queue, say — ends the process at startup
  with an error naming it, rather than idling forever against a queue it can never read.
- **Ctrl+C stops new receives**, then the in-flight batch finishes and settles within the
  shutdown grace period.
- `QueueMessage::timestamp_ms` is the moment the batch was *received*: the portable
  `ReceivedMessage` carries no enqueue time on the pull path, so unlike Cloudflare's pushed batch
  it is not an enqueue timestamp. Redelivery counts are logged by the consumer rather than carried
  on the message.

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
| Message Queue | [`SqsQueue`](../aws/) | `CfQueue` | `SqsQueue` | `ServiceBusQueue` | `InMemoryQueue` |
| Portable SQL | `Db` via sqlx | `Db` via D1 | planned wiring | planned wiring | — |

The **Native** column names what a native deployment can wire, not a separate implementation.
Runtime and provider are independent axes, so a backend that is a plain HTTP client — `SqsQueue`,
`DynamoKv`, `S3Storage` — is reachable from a native server too; `SqsQueue` appears twice for that
reason, and `[native.service.*]` wiring with `backend = "sqs"` builds exactly it.

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

### Schema Changes

Schema is managed with plain `.sql` files, embedded at compile time and applied by a runner that
works on every backend `Db` works on:

```rust
use skyzen::embed_migrations;
use skyzen_services::Migrations;

static MIGRATIONS: Migrations = embed_migrations!("migrations");

db.migrate(&MIGRATIONS).await?;
```

The same files are what `skyzen migrate` applies from the CLI, and what
`#[skyzen::test(migrations = MIGRATIONS)]` applies to a test's database. See the
[Migrations Guide](migrations.md) for the file-naming rules, per-backend atomicity, the checksum
policy that detects an edited migration, and the CLI and testing workflows.

For object-scoped SQL that runs on both native and Cloudflare, continue with the [Durable Object + SQL Guide](durable-sql-guide.md).
