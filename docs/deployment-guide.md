# Deploying Skyzen Apps

Skyzen apps can be deployed to native servers, Cloudflare Workers, AWS Lambda, and Azure Functions. This guide covers each target platform.

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

### Docker

```dockerfile
FROM rust:1.82-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
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

This generates `.skyzen/gen/wrangler.toml` from your `Skyzen.toml` and runs `wrangler dev`.

### Deploy

```sh
skyzen deploy --provider cloudflare
```

### Manual Build

If not using the Skyzen CLI:

```sh
cargo build --target wasm32-unknown-unknown --release
wasm-bindgen --target web target/wasm32-unknown-unknown/release/my_app.wasm --out-dir dist
```

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

## AWS Lambda

### Project Setup

Use the same `cdylib` setup as Cloudflare Workers. The SAM template defines the Lambda function:

**`template.yaml`:**

```yaml
AWSTemplateFormatVersion: '2010-09-09'
Transform: AWS::Serverless-2016-10-31

Globals:
  Function:
    Timeout: 30

Resources:
  MyFunction:
    Type: AWS::Serverless::Function
    Properties:
      Runtime: provided.al2023
      Handler: bootstrap
      CodeUri: target/release/
      Events:
        Api:
          Type: Api
          Properties:
            Path: /{proxy+}
            Method: ANY
```

### Skyzen.toml

```toml
[aws]
template = "template.yaml"
stack_name = "my-app-stack"
region = "us-east-1"
local_port = 3001
```

### Local Development

```sh
skyzen dev --provider aws
```

Runs `sam local start-api` with the configured template and port.

### Deploy

```sh
skyzen deploy --provider aws
```

Runs `sam deploy` with the configured stack name and region.

## Azure Functions

### Skyzen.toml

```toml
[azure]
project = "."
app_name = "my-function-app"
port = 7071
```

### Local Development

```sh
skyzen dev --provider azure
```

Runs `func start` in the project directory.

### Deploy

```sh
skyzen deploy --provider azure
```

Runs `func azure functionapp publish <app_name>`.

## Running from Source

You can also run the CLI from the Skyzen repository:

```sh
cargo run -p skyzen-cli -- dev --provider cloudflare --manifest ./Skyzen.toml
cargo run -p skyzen-cli -- deploy --provider aws --manifest ./Skyzen.toml
cargo run -p skyzen-cli -- doctor
```

Use `--dry-run` to preview generated config files without writing them:

```sh
cargo run -p skyzen-cli -- dev --provider cloudflare --manifest ./Skyzen.toml --dry-run
```

## Dual-Target Development

A common workflow is to develop natively and deploy to WASM:

1. **Develop** with `cargo run` (native) — get fast compile times, logging, and `Ctrl+C`
2. **Test** with `skyzen-test` mocks — no external services needed
3. **Preview** with `skyzen dev --provider cloudflare` — local Workers emulation
4. **Deploy** with `skyzen deploy --provider cloudflare` — production

The same `#[skyzen::main]` entry point works for both native and WASM targets. Your handler code never changes.
