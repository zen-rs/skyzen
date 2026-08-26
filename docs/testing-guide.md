# Testing with Skyzen

Skyzen provides a dedicated testing crate (`skyzen-test`) with mock service implementations, an HTTP test client, response assertions, and snapshot testing — everything you need to test handlers without network I/O or external services.

## Setup

```toml
[dev-dependencies]
skyzen-test = { version = "0.1" }

# Enable a runtime feature for InMemoryDb (SQLite) support:
# skyzen-test = { version = "0.1", features = ["runtime-tokio-rustls"] }
```

## Mock Services

All mocks are in-memory, isolated per instance, and implement the same service traits as production backends.

### `InMemoryKv`

In-memory `KeyValueStore` implementation:

```rust
use skyzen_test::mock::InMemoryKv;
use skyzen_services::Kv;

let mock = InMemoryKv::new();
let kv = Kv::new(mock);

kv.put("key", b"value").await.unwrap();
let val = kv.get("key").await.unwrap();
assert_eq!(val, Some(b"value".to_vec()));

// The atomic primitives are real, not stubs: each runs under the store's
// write lock, so a lock or a rate limiter behaves the way it will on Redis.
assert!(kv.put_if_absent("lock", b"held").await.unwrap());
assert!(!kv.put_if_absent("lock", b"stolen").await.unwrap());
assert_eq!(kv.increment("hits", 1).await.unwrap(), 1);
```

**TTL fidelity.** By default the mock honours whatever `Duration` it is given, which is *more*
permissive than production: a test that stores a 5-second nonce and asserts it has expired passes
here while Cloudflare KV keeps that nonce alive for a full minute. Use
`InMemoryKv::strict_cloudflare()` — or `InMemoryKv::new().with_min_ttl(d)` for another platform's
floor — when the code under test depends on a short expiry.

### `InMemoryStorage`

In-memory `ObjectStorage` implementation:

```rust
use skyzen_test::mock::InMemoryStorage;
use skyzen_services::Storage;

let mock = InMemoryStorage::new();
let storage = Storage::new(mock);

storage.put("file.txt", b"hello".to_vec()).await.unwrap();
let obj = storage.get("file.txt").await.unwrap().unwrap();
assert_eq!(obj.body, b"hello");

// The byte path is implemented too: streams, real range arithmetic, and a
// presigned URL that is deterministic and deliberately *not* fetchable.
let slice = storage.get_range("file.txt", ByteRange::slice(1, 3)).await.unwrap().unwrap();
assert_eq!(slice.body, b"ell");
assert_eq!(slice.metadata.size, 5); // still the whole object, for Content-Range
```

`presign_get`/`presign_put` return a `memory://` URL: nothing serves that scheme, so a test that
accidentally follows one fails at connect time instead of reaching a real bucket.

### `InMemoryQueue`

In-memory `MessageQueue` implementation:

```rust
use skyzen_test::mock::InMemoryQueue;
use skyzen_services::Queue;

let mock = InMemoryQueue::new();
let queue = Queue::new(mock);

queue.send(b"message").await.unwrap();

// Consumption is modelled the way a pull-based broker works, not as a plain
// pop: a receive leases the message and hides it for the visibility timeout,
// `ack` deletes it, `nack` reschedules it, and every delivery bumps `attempts`.
let received = queue.receive(ReceiveOptions::new()).await.unwrap();
assert_eq!(received[0].attempts, Some(1));
queue.ack(&received[0].receipt).await.unwrap();
```

The mock never sleeps — it has no runtime to long-poll with — so `ReceiveOptions::wait` returns
whatever is visible immediately. Drive expiry with a zero or very short visibility timeout rather
than by sleeping.

### `InMemoryDb`

SQLite in-memory database for SQL tests. Requires a runtime feature (`runtime-tokio-rustls` or `runtime-tokio-native-tls`).

```rust
use skyzen_test::mock::InMemoryDb;

let db = InMemoryDb::with_schema(
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
).await.unwrap();

db.db()
    .query("INSERT INTO users (name) VALUES (?)")
    .bind("alice")
    .execute()
    .await
    .unwrap();
```

## TestClient

`TestClient` sends HTTP requests directly to an endpoint without network I/O. Create it via `TestContext`:

```rust
use skyzen_test::{TestContext, TestClient};

let ctx = TestContext::new();
let client = ctx.client(my_router);
```

### Building Requests

```rust
// GET request
let response = client.get("/users").send().await;

// POST with JSON body
let response = client
    .post("/users")
    .json(&serde_json::json!({"name": "alice"}))
    .send()
    .await;

// PUT with custom headers
let response = client
    .put("/users/1")
    .header("X-Custom", "value")
    .json(&update)
    .send()
    .await;

// DELETE with bearer auth
let response = client
    .delete("/users/1")
    .bearer("my-token")
    .send()
    .await;

// PATCH with raw body
let response = client
    .patch("/data")
    .body("raw bytes")
    .send()
    .await;

// HEAD and OPTIONS — the latter is what exercises a CORS preflight,
// which router-wide layers answer.
let response = client.head("/users/1").send().await;
let response = client.options("/users").send().await;

// Any other method, including one chosen at runtime
let response = client.request(Method::TRACE, "/debug").send().await;
```

Every verb helper delegates to `request(method, path)`, so a route registered for a method the
helpers do not name is still reachable.

## Injecting Services

`TestContext` carries all seven portable services into every request its clients send:

```rust
let ctx = TestContext::new()
    .with_kv(Kv::new(InMemoryKv::new()))
    .with_storage(Storage::new(InMemoryStorage::new()))
    .with_queue(Queue::new(InMemoryQueue::new()))
    .with_db(db)
    .with_durable_kv(DurableKv::new(InMemoryDurableKv::new()))
    .with_durable_db(DurableDb::new(InMemoryDurableDb::new()))
    .with_alarm(Alarm::new(InMemoryAlarm::new()));
```

`#[skyzen::test]` does this for you: name any of `Kv`, `Storage`, `Queue`, `Db`, `DurableKv`,
`DurableDb`, `Alarm`, a generated database wrapper, or `TestContext` as a parameter and the macro
constructs the mock, hands it to the test, and forwards it into the context. A `TestContext`
parameter on its own provisions all of them, so a handler under test can extract whichever it
needs:

```rust
#[skyzen::test]
async fn alarm_is_scheduled(durable_kv: DurableKv, alarm: Alarm, ctx: TestContext) {
    ctx.client(app()).post("/schedule").send().await.assert_status(200);

    // The handler and the test share the same mocks.
    assert_eq!(alarm.get_alarm().await.unwrap(), Some(1337));
    assert!(durable_kv.get("scheduled").await.unwrap().is_some());
}
```

## Response Assertions

Every `send()` returns a `TestResponse` with rich assertion methods:

### Status Assertions

```rust
response.assert_status(200);           // Exact status code
response.assert_status_success();      // Any 2xx
response.assert_status_client_error(); // Any 4xx
response.assert_status_server_error(); // Any 5xx
```

### Header Assertions

```rust
response.assert_header("content-type", "application/json");
response.assert_header_exists("x-request-id");
```

### Body Assertions

```rust
response.assert_body_contains("hello");

// Deserialize JSON body
let user: User = response.assert_json();

// Assert a specific JSON path
response.assert_json_path("data.id", &serde_json::json!(42));
response.assert_json_path("users.0.name", &serde_json::json!("alice"));
```

### Accessors

```rust
let status = response.status();
let headers = response.headers();
let bytes = response.body_bytes();
let text = response.body_text();
let user: User = response.json();
```

## Snapshot Testing

The `SnapshotExt` trait adds snapshot assertions powered by [`insta`](https://insta.rs):

```rust
use skyzen_test::SnapshotExt;

let response = client.get("/api/users").send().await;
response.assert_snapshot("list_users_response");
```

JSON responses are automatically pretty-printed in snapshots. Manage snapshots with `cargo insta review`.

## Fixture Loading

Load test data from JSON strings:

```rust
use skyzen_test::fixtures::from_json_str;

let user: User = from_json_str(r#"{"id": 1, "name": "alice"}"#).unwrap();
let users: Vec<User> = from_json_str(r#"[{"id": 1, "name": "alice"}]"#).unwrap();
```

## Full Example

Here's a complete integration test combining mocks, a test client, and assertions:

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::utils::Json;
use skyzen_services::Kv;
use skyzen_test::{TestContext, SnapshotExt};
use skyzen_test::mock::InMemoryKv;

// The handler under test — identical to production code
async fn get_greeting(kv: Kv) -> Result<String> {
    let name = kv.get_text("user:name").await?.unwrap_or("World".into());
    Ok(format!("Hello, {name}!"))
}

fn app(kv: Kv) -> Router {
    Route::new((
        "/greeting".at(get_greeting),
    ))
    .with(kv)
    .build()
}

#[tokio::test]
async fn test_greeting_with_stored_name() {
    // Set up mock
    let kv = Kv::new(InMemoryKv::new());
    kv.put("user:name", b"Skyzen").await.unwrap();

    // Build app with mock
    let ctx = TestContext::new();
    let client = ctx.client(app(kv));

    // Send request and assert
    let response = client.get("/greeting").send().await;
    response.assert_status(200);
    response.assert_body_contains("Hello, Skyzen!");
}

#[tokio::test]
async fn test_greeting_default() {
    let kv = Kv::new(InMemoryKv::new());

    let ctx = TestContext::new();
    let client = ctx.client(app(kv));

    let response = client.get("/greeting").send().await;
    response.assert_status(200);
    response.assert_body_contains("Hello, World!");
}
```

The key insight: **the handler code is production code**. Only the wiring in the test uses `InMemoryKv` instead of `Redis` or `DynamoKv`. This gives you confidence that the same handler will work correctly in production.
