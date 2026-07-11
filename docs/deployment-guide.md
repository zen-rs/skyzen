# Deploying Skyzen Apps

Skyzen is production-complete for native servers and Cloudflare Workers. This guide covers those paths directly and also documents the current AWS/Azure CLI orchestration hooks, which are not runtime-parity features.

## Prerequisites

Run `skyzen doctor` to verify your toolchain:

```sh
skyzen doctor
```

This checks for platform-specific tools (`wrangler`, `sam`, `func`) and reports missing dependencies.

## Native Deployment

### Building

```sh
cargo build --release
```

The binary is at `target/release/<your-app>`. It includes:
- Pretty logging via `tracing` (controlled by `RUST_LOG`)
- Graceful shutdown on `Ctrl+C`
- CLI overrides: `--port`, `--host`, `--listen`

### Running

```sh
./target/release/my-app --port 8080
```

Without `--port` or `SKYZEN_ADDRESS`, Skyzen binds an available localhost port and logs the final address.

### Docker

```dockerfile
FROM rust:1.82-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
ENV SKYZEN_ADDRESS=0.0.0.0:8787
COPY --from=builder /app/target/release/my-app /usr/local/bin/
EXPOSE 8787
CMD ["my-app"]
```

## Cloudflare Workers

### Project Setup

Use a `lib` crate with `cdylib` output:

**`Cargo.toml`:**

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
skyzen = { version = "0.1", default-features = false, features = ["json"] }
```

**`src/lib.rs`:**

```rust
#[skyzen::main]
fn app() -> Router {
    Route::new((
        "/".at(|| async { "Hello from Workers!" }),
    ))
    .build()
}
```

On WASM targets, `#[skyzen::main]` exports a WinterCG-compatible `fetch` handler.

### Skyzen.toml

```toml
[cloudflare]
name = "my-worker"
main = "dist/worker.js"
compatibility_date = "2025-02-01"
workers_dev = true

[[cloudflare.kv_namespaces]]
binding = "CACHE"
id = "your-kv-id"
```

See the [Skyzen.toml Reference](skyzen-toml-reference.md) for all Cloudflare options.

### Local Development

```sh
skyzen dev --provider cloudflare
```

This now performs the full Worker preparation flow:

1. `cargo build --target wasm32-unknown-unknown --lib`
2. Skyzen generates WebAssembly bindings internally
3. Skyzen writes:
   - `dist/worker.js`
   - `dist/worker_bg.js`
   - `dist/worker_bg.wasm`
4. Skyzen generates `.skyzen/gen/wrangler.toml`
5. Skyzen runs `wrangler dev --local`

No manual `wasm-bindgen` step is required in application projects.

### Deploy

```sh
skyzen deploy --provider cloudflare
```

`skyzen deploy --provider cloudflare` uses the same formal Cloudflare build pipeline before invoking `wrangler deploy`.

### Durable Objects

For Durable Objects with SQLite storage, configure migrations in `Skyzen.toml`:

```toml
[[cloudflare.durable_objects.bindings]]
name = "STATE"
class_name = "AppState"

[[cloudflare.durable_objects.migrations]]
tag = "v1"
new_sqlite_classes = ["AppState"]
```

Bump the migration `tag` (`v2`, `v3`, ...) whenever class definitions change.

## Running from Source

You can also run the CLI from the Skyzen repository:

```sh
cargo run -p skyzen-cli -- dev --provider cloudflare --manifest ./Skyzen.toml
cargo run -p skyzen-cli -- deploy --provider cloudflare --manifest ./Skyzen.toml
cargo run -p skyzen-cli -- doctor
```

Use `--dry-run` to preview generated config files without writing them:

```sh
cargo run -p skyzen-cli -- dev --provider cloudflare --manifest ./Skyzen.toml --dry-run
```

For Cloudflare, `--dry-run` also prints the internal build step Skyzen will perform before invoking Wrangler.

## Dual-Target Development

A common workflow is to develop natively and deploy to WASM:

1. **Develop** with `cargo run` (native) — get fast compile times, logging, and `Ctrl+C`
2. **Test** with `skyzen-test` mocks — no external services needed
3. **Preview** with `skyzen dev --provider cloudflare` — local Workers emulation
4. **Deploy** with `skyzen deploy --provider cloudflare` — production

The same `#[skyzen::main]` entry point works for both native and WASM targets. Your handler code never changes.
