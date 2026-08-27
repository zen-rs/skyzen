# skyzen-test

[![Crates.io](https://img.shields.io/crates/v/skyzen-test.svg)](https://crates.io/crates/skyzen-test)
[![Documentation](https://docs.rs/skyzen-test/badge.svg)](https://docs.rs/skyzen-test)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](#license)

Testing utilities and mock implementations for the Skyzen HTTP framework.

## Overview

`skyzen-test` provides a suite of tools designed to make testing Skyzen applications simple, fast, and reliable. It includes mock implementations for all core services, an in-memory HTTP client for integration testing without network overhead, and a rich set of response assertions.

## Mock Services

This crate provides platform-agnostic, in-memory implementations of the `skyzen-services` traits. These mocks allow you to test your handlers and business logic without needing real infrastructure like Redis, S3, or SQS.

- **`InMemoryKv`**: A `DashMap`-based implementation of `KeyValueStore`, atomics included.
- **`InMemoryStorage`**: In-memory `ObjectStorage` for file upload/download simulation, including the streaming, range and presign surface.
- **`InMemoryQueue`**: In-memory `MessageQueue` for testing background workers, with the full `receive`/`ack`/`nack` lease behaviour.
- **`InMemoryDb`**: SQLite in-memory database implementation (requires `runtime-tokio-native-tls` or `runtime-tokio-rustls` feature). `InMemoryDb::with_schema` takes raw DDL; `InMemoryDb::with_migrations` runs your real migration set through the production runner, so a migration that would fail on deploy fails in the test suite instead.
- **`InMemoryDurableKv`, `InMemoryDurableDb`, `InMemoryAlarm`**: the Durable Object surface, so a Workers application is testable on a plain native `cargo test`.

## HTTP Testing

### TestContext & TestClient

The `TestContext` is automatically injected into functions annotated with `#[skyzen::test]`. The macro also provides in-memory `Kv`, `Storage`, `Queue`, `Db`, `DurableKv`, `DurableDb` and `Alarm` values when those parameter types appear — and `#[skyzen::test(migrations = MIGRATIONS)]` applies your migration set to the database first. `TestContext` forwards every service it holds into each `TestClient` request it creates.

Built by hand, the same slots are `with_kv`, `with_storage`, `with_queue`, `with_db`, `with_durable_kv`, `with_durable_db` and `with_alarm`.

The `TestClient` executes requests directly against your application's router, bypassing the network stack for maximum performance and easier debugging. `get`, `post`, `put`, `patch`, `delete`, `head` and `options` cover the usual verbs; `request(method, path)` covers anything else.

```rust
use skyzen_test::TestContext;

#[skyzen::test]
async fn test_api(ctx: TestContext) {
    // Create a client for your app's router
    let client = ctx.client(my_app_router());

    // Build and send requests
    let response = client.get("/api/users/1")
        .header("Accept", "application/json")
        .bearer("my-token")
        .send()
        .await;

    // Assertions
    response.assert_status(200);
}
```

### Response Assertions

`TestResponse` provides a fluent API for asserting the state of a response:

- **Status**: `assert_status(code)`, `assert_status_success()`, `assert_status_client_error()`, `assert_status_server_error()`.
- **Headers**: `assert_header(name, value)`, `assert_header_exists(name)`.
- **Body**: `assert_body_contains(str)`, `assert_json<T>()`, `assert_json_path(path, expected)`.

### Snapshot Testing

Integration with the [`insta`](https://crates.io/crates/insta) crate allows for easy snapshot testing of response bodies.

```rust
use skyzen_test::SnapshotExt;

// ...
response.assert_snapshot("user_profile");
```

## Fixture Loading

Helpers for loading test data from JSON fixtures:

```rust
use skyzen_test::fixtures::from_json_str;

let user: User = from_json_str(include_str!("../fixtures/user.json")).unwrap();
```

## Feature Flags

- **`runtime-tokio-native-tls`**: Enables `InMemoryDb` (SQLite) using `tokio` and `native-tls`.
- **`runtime-tokio-rustls`**: Enables `InMemoryDb` (SQLite) using `tokio` and `rustls`.

## Full Example

A complete example demonstrating a handler that uses a KV store, being tested with a mock.

```rust
use skyzen::routing::{CreateRouteNode, Route, Router};
use skyzen::utils::Json;
use skyzen::{Responder, StatusCode};
use skyzen_services::Kv;
use skyzen_test::TestContext;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
struct User {
    id: String,
    name: String,
}

// Handler that saves a user to KV
async fn create_user(kv: Kv, Json(user): Json<User>) -> impl Responder {
    let key = format!("user:{}", user.id);
    let value = serde_json::to_vec(&user).unwrap();
    kv.put(&key, value).await.unwrap();
    StatusCode::CREATED
}

fn app() -> Router {
    Route::new((
        "/users".post(create_user),
    ))
    .build()
}

#[skyzen::test]
async fn test_create_user(kv: Kv, ctx: TestContext) {
    // 1. Arrange
    let client = ctx.client(app());
    let new_user = User { id: "123".into(), name: "Alice".into() };

    // 2. Act
    let resp = client.post("/users").json(&new_user).send().await;

    // 3. Assert
    resp.assert_status(201);

    // Verify state in the mock KV
    let stored = kv.get("user:123").await.unwrap().unwrap();
    let decoded: User = serde_json::from_slice(&stored).unwrap();
    assert_eq!(decoded.name, "Alice");
}
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
