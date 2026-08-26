# skyzen-macros

[![crates.io](https://img.shields.io/crates/v/skyzen-macros.svg)](https://crates.io/crates/skyzen-macros)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-macros)
[![License](https://img.shields.io/crates/l/skyzen-macros.svg)](../LICENSE)

Procedural macros powering the Skyzen web framework.

## Overview

This crate provides the procedural macros used by Skyzen. **Import them through the `skyzen` crate** — you should not depend on `skyzen-macros` directly.

## Macros

### `#[skyzen::main]`

Boots a Skyzen endpoint on native or WASM runtimes.

```rust
#[skyzen::main]
fn main() -> Router {
    Route::new(("/".at(|| async { "Hello!" }),)).build()
}
```

**On native targets**: Sets up Tokio + Hyper, `tracing` logging, graceful shutdown, and CLI argument parsing (`--port`, `--host`, `--listen`).

**On WASM targets**: Exports a WinterCG-compatible `fetch` handler.

Options:
- `default_logger = false` — Disable the built-in tracing subscriber

### `#[skyzen::queue]`

Exports a Cloudflare queue-consumer entrypoint on wasm targets while leaving the original async function available for normal Rust use:

```rust
#[cfg(target_arch = "wasm32")]
#[skyzen::queue]
async fn queue(
    batch: skyzen_cloudflare::CfQueueBatch,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfEventContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    batch.ack_all()?;
    Ok(())
}
```

### `#[skyzen::scheduled]`

Exports a Cloudflare scheduled/cron entrypoint on wasm targets:

```rust
#[cfg(target_arch = "wasm32")]
#[skyzen::scheduled]
async fn scheduled(
    event: skyzen_cloudflare::CfScheduledEvent,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfScheduleContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    Ok(())
}
```

### `#[skyzen::email]`

Exports a Cloudflare Email Worker entrypoint on wasm targets, for mail routed to the Worker:

```rust
#[cfg(target_arch = "wasm32")]
#[skyzen::email]
async fn email(
    message: skyzen_cloudflare::CfEmailMessage,
    env: skyzen::runtime::wasm::Env,
    ctx: skyzen_cloudflare::CfEventContext,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    message.forward("archive@lexo.cool").await
}
```

The first argument must be `CfEmailMessage`.

### `#[skyzen::tail]`

Exports a Cloudflare Tail Worker entrypoint on wasm targets, receiving another Worker's logs and
exceptions:

```rust
#[cfg(target_arch = "wasm32")]
#[skyzen::tail]
async fn tail(
    traces: Vec<skyzen_cloudflare::TailTraceItem>,
    env: skyzen::runtime::wasm::Env,
) -> Result<(), skyzen_cloudflare::CfEventError> {
    Ok(())
}
```

The first argument is either `CfTailEvent` (the raw batch) or `Vec<TailTraceItem>` (decoded).

### `#[skyzen::durable_object]`

Applied to a `DurableObject` impl block, this exports a Cloudflare Durable Object class wrapper that forwards `fetch`, `alarm`, and hibernation websocket events into Skyzen's `DurableObjectRuntime`.

### `#[skyzen::openapi]`

Annotates a handler for OpenAPI documentation generation.

```rust
#[skyzen::openapi]
async fn get_user(params: Params) -> Result<Json<User>> {
    // ...
}
```

Extracts request/response schemas from the function signature and registers the operation at compile time via `linkme` distributed slices on native builds with the `openapi` feature enabled.

### `#[skyzen::test]`

Runs an async test on Skyzen's native test runtime and injects test helpers such as `TestContext`, `Kv`, `Storage`, `Queue`, and `Db` using in-memory implementations.

### `#[skyzen::error]`

Defines custom HTTP error types with status codes.

### `#[derive(HttpError)]`

Derive macro for enums to implement HTTP error responses.

### `import_config!()`

Generates typed datasource code from `Skyzen.toml` declarations:

```rust
skyzen::import_config!();
```

Reads `[[datasource]]` entries from `Skyzen.toml` and generates types, `init()` functions, and middleware for each datasource. Automatically expanded by `#[skyzen::main]`.

## License

MIT or Apache-2.0, at your option.
