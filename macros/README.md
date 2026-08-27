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

**On native targets**: reads the environment before binding anything. `AWS_LAMBDA_RUNTIME_API` hands
over to the Lambda adapter (which needs the root crate's `lambda` feature, and says so rather than
binding a port nothing in Lambda can reach); `FUNCTIONS_CUSTOMHANDLER_PORT` serves the Azure
Functions host on the port it chose, with the manifest's `[[azure.queue_triggers]]` mounted in front
of the router; neither means an ordinary server, with Tokio + Hyper, `tracing` logging, graceful
shutdown and CLI argument parsing (`--port`, `--host`, `--listen`).

**On WASM targets**: Exports a WinterCG-compatible `fetch` handler.

It also expands `import_config!()` and, when `Skyzen.toml` declares `[[native.queue_consumer]]`,
starts the polling loops that drive `#[skyzen::queue]` beside the HTTP server.

Options:
- `default_logger = false` — Disable the built-in tracing subscriber

### `#[skyzen::queue]`

Marks the function that consumes a queue batch. **It is dual-target**: the same handler is invoked
by Skyzen's own polling loop natively (`[[native.queue_consumer]]`), by the platform's push on
Cloudflare Workers and on AWS Lambda (an SQS event source), and by the Azure Functions host
(`[[azure.queue_triggers]]`). The portable shape takes a `QueueBatch<T>` and returns `()`,
`Result<(), _>` or a `QueueBatchDisposition`:

```rust
#[skyzen::queue]
async fn queue(batch: QueueBatch<Job>) -> QueueBatchDisposition {
    QueueBatchDisposition::ack_all()
}
```

A handler that nothing consumes — no `[[native.queue_consumer]]` and no
`[[cloudflare.queues.consumers]]` — is a compile error rather than dead code.

The wasm-specific form below takes Cloudflare's own types instead, and exports the Worker's `queue`
entrypoint while leaving the original async function available for normal Rust use:

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

Runs an async test on Skyzen's native test runtime and injects test helpers — `TestContext`, `Kv`,
`Storage`, `Queue`, `Db`, `DurableKv`, `DurableDb`, `Alarm`, and the named extractors the manifest
declares — using in-memory implementations. `migrations = <path to a Migrations static>` applies a
migration set to the injected database first, through the production runner:

```rust
#[skyzen::test(migrations = MIGRATIONS)]
async fn a_user_can_be_inserted(db: Db, ctx: TestContext) { /* ... */ }
```

### `#[skyzen::error]`

Defines custom HTTP error types with status codes. On a struct, `message = "..."` is the `Display`
format; on an enum, every variant carries its own `#[error("...", status = ...)]` and an
enum-level `message` is **rejected with a compile error** rather than silently ignored.

### `#[derive(HttpError)]`

Derive macro for enums to implement HTTP error responses.

### `embed_migrations!("migrations")`

Reads a `<version>_<name>.sql` directory relative to `CARGO_MANIFEST_DIR` at compile time,
validates it, checksums each file and emits `include_str!` for the contents, producing a
`skyzen_services::Migrations`:

```rust
static MIGRATIONS: Migrations = skyzen::embed_migrations!("migrations");
```

A malformed file name or a repeated version is a compile error pointing at the path literal. It
reads through the same `skyzen-manifest` module the `skyzen migrate` CLI uses, so the binary and
the CLI can never disagree about which files count or how a checksum is computed.

### `import_config!()`

Generates typed service and database extractors from `Skyzen.toml` declarations:

```rust
skyzen::import_config!();
```

Reads the `[[service]]` and `[[database]]` entries — through the shared
[`skyzen-manifest`](../manifest) schema, the same one the `skyzen` CLI parses with — and generates
a named newtype, a `*NotConfigured` error, an `Extractor` and a `Middleware` for each. Automatically
expanded by `#[skyzen::main]`, which also generates the backend construction from the matching
`[native.*]` / `[cloudflare.*]` wiring.

## License

MIT or Apache-2.0, at your option.
