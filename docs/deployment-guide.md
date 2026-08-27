# Deploying Skyzen Apps

Skyzen deploys the same application four ways: as a native server, as a Cloudflare Worker, as an
AWS Lambda function, and as an Azure Functions custom handler. `--provider` accepts `native`,
`cloudflare`, `aws` and `azure`.

Nothing in an application is annotated for a platform. On native targets the runtime reads the
environment before it binds anything: `AWS_LAMBDA_RUNTIME_API` means it is inside Lambda and hands
over to the Lambda adapter, `FUNCTIONS_CUSTOMHANDLER_PORT` means the Azure Functions host is waiting
on a port it chose, and neither means an ordinary server. The wasm32 build is the Worker. One
`#[skyzen::main]`, one set of handlers, four deployments.

## Prerequisites

Run `skyzen doctor` to check your toolchain and your manifest:

```sh
skyzen doctor
skyzen doctor --provider cloudflare
```

An unqualified `skyzen doctor` checks `native` and `cloudflare`, the two that need no cloud
account; pass `--provider aws` or `--provider azure` to check those. It reports, per selected
provider:

- that the tools that provider shells out to are on `PATH`, and how to install one that is not
  (`wrangler` for Cloudflare, `cargo lambda` for AWS, `func` for Azure);
- that the `wasm32-unknown-unknown` target is installed;
- that `Skyzen.toml` parses, and that every portable `[[service]]` / `[[database]]` has both its
  `[cloudflare.*]` wiring and a matching binding entry;
- that the application's resolved `wasm-bindgen` matches the bindings generator the CLI embeds
  (they share a schema version and must be exactly equal);
- that `wrangler whoami` succeeds, so a deploy will not fail on authentication;
- for AWS, what `[aws]` would deploy — the function name, the architecture, whether it gets a
  Function URL;
- for Azure, that `[azure]` names an `app_name` to publish to, and that the Linux target it names
  is installed.

Every check runs even after one fails, and the command exits non-zero if any did.

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

# Must equal the generator `skyzen` embeds — `skyzen doctor` reports the version to use, and
# `skyzen new` writes this line for you.
[target.'cfg(target_arch = "wasm32")'.dependencies]
wasm-bindgen = "=0.2.120"
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

Leave `id` out of a KV or D1 entry and let Skyzen create the resource:

```sh
skyzen provision --provider cloudflare --dry-run   # print the plan
skyzen provision --provider cloudflare
```

Provisioning runs wrangler's create subcommands and writes the returned ids back into
`Skyzen.toml` with a format-preserving edit, so comments and layout survive. It skips anything
that already has an id, which makes re-running it harmless, and it probes `wrangler whoami` first
rather than failing halfway through.

See the [Skyzen.toml Reference](skyzen-toml-reference.md) for all Cloudflare options.

### Local Development

```sh
skyzen dev --provider cloudflare
```

This performs the full Worker preparation flow:

1. Check the application's `wasm-bindgen` against the embedded generator
2. `cargo build --target wasm32-unknown-unknown --lib`
3. Skyzen generates WebAssembly bindings internally
4. Skyzen writes:
   - `dist/worker.js`
   - `dist/worker_bg.js`
   - `dist/worker_bg.wasm`
5. Skyzen generates `.skyzen/gen/wrangler.toml`
6. Skyzen runs `wrangler dev --local`

No manual `wasm-bindgen` step is required in application projects.

Skyzen then watches the project. A source change re-runs steps 2–4 while `wrangler dev` stays up;
wrangler reloads the regenerated bundle on its own, so local state and the bound port survive.
A binding with no provisioned id binds a local namespace, so a freshly scaffolded project runs
before it has any cloud resources.

Trailing arguments reach the runner, which is how wrangler-only flags are used:

```sh
skyzen dev --provider cloudflare -- --test-scheduled   # fire the cron handler on demand
skyzen dev -- --port 3000                              # native: passed to the application
```

### Build

Produce the artifacts without running or deploying anything — for CI packaging, or to inspect the
bundle:

```sh
skyzen build --provider cloudflare
skyzen build --provider cloudflare --release
```

Every build prints the final `.wasm` size, raw and gzipped. Cloudflare enforces a *compressed*
size limit, so the gzipped figure is the one that decides whether an upload is accepted. Release
builds are run through `wasm-opt -Os` first, using the binaryen linked into the CLI — there is no
`wasm-opt` to install.

### Deploy

```sh
skyzen deploy --provider cloudflare
skyzen deploy --provider cloudflare --dry-run
```

`deploy` uses the same build pipeline before invoking `wrangler deploy`. `--dry-run` runs the real
build and passes `--dry-run` to wrangler, so it validates the actual bundle rather than skipping
the work. A binding with no provisioned id is refused here rather than being invented.

### Environments

`[cloudflare.env.<name>]` overlays the base configuration; `--env <name>` selects it and forwards
to wrangler:

```sh
skyzen deploy --provider cloudflare --env staging
skyzen logs --env staging
```

See [Environments](skyzen-toml-reference.md#environments) for the merge rules.

### Logs and Secrets

```sh
skyzen logs                                  # wrangler tail
skyzen logs -- --format json                 # extra arguments pass straight through
skyzen secret set API_KEY                    # value is read from stdin
skyzen secret list
```

Secrets are the channel for anything sensitive; `[cloudflare.vars]` is plaintext and committed.

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

### SQL Migrations

SQL migrations live in `migrations/` — wrangler's `migrations_dir` default, and Skyzen's
`[[database]].migrations_dir` default, so both paths read the same files. Apply them to every
declared `[[cloudflare.d1_databases]]`:

```sh
skyzen migrate --local     # the emulator's database, for `skyzen dev`
skyzen migrate             # the deployed database
skyzen migrate --env staging
skyzen migrate status      # `wrangler d1 migrations list` — what is applied, what is pending
```

The same files apply to a native database without wrangler:

```sh
skyzen migrate --provider native          # through [native.database.<name>].url_env
skyzen migrate status --provider native   # reads the `_skyzen_migrations` table
```

`status` follows the same `--provider` as applying does, so it always reports on the database
`skyzen migrate` would write to.

The full rules — file naming, per-backend atomicity, the checksum policy that refuses an edited
migration, and how an application embeds the same set with `skyzen::embed_migrations!` — are in the
[Migrations Guide](migrations.md).

## AWS Lambda

### What runs

The Lambda adapter serves both kinds of invocation a Skyzen application can receive:

- **HTTP** — Function URLs, API Gateway (REST and HTTP APIs), ALB and VPC Lattice. `lambda_http`
  normalizes each event shape into a request, the application's router answers it, and the response
  goes back in the shape that caller expects. A body that is valid UTF-8 travels as text and
  anything else as base64, so a compressed response arrives intact.
- **SQS** — a batch is decoded into the same `QueueBatch` a `#[skyzen::queue]` handler receives
  natively and on Workers, including the `skyzen-content-encoding` tag `SqsQueue` writes for a
  binary payload. The reply is a [partial batch response][batch]: only the messages the handler
  failed are redelivered.

An SQS event that reaches a Lambda with no `#[skyzen::queue]` handler fails the invocation by name
rather than acknowledging messages nothing processed.

[batch]: https://docs.aws.amazon.com/lambda/latest/dg/services-sqs-errorhandling.html

### Enabling the adapter

The adapter is an optional feature, because it brings its own Tokio runtime:

```toml
[dependencies]
skyzen = { version = "0.1", features = ["lambda"] }
```

Without it, a binary that finds itself inside Lambda **refuses to start** and names this feature —
rather than binding a TCP port nothing in Lambda can reach and timing out on every invocation.

### Configuring

```toml
[aws]
function_name = "skyzen-api"     # defaults to the binary's own name
memory_mb = 512                  # Lambda scales CPU with memory
timeout = "30s"                  # humantime; sent to Lambda as seconds
architecture = "arm64"           # or "x86_64"; arm64 is the default and the cheaper one
url = true                       # create (and keep) a Function URL; the default

[aws.env]
RUST_LOG = "info"
```

`[aws.env]` values are plaintext environment variables, visible to anyone who can read the
function's configuration. Read anything sensitive from Secrets Manager or SSM at startup instead.

### Deploying

```sh
cargo install cargo-lambda
skyzen deploy --provider aws
```

That runs, and `--dry-run` prints, exactly:

```sh
cargo lambda build --target aarch64-unknown-linux-gnu --release --features lambda
cargo lambda deploy --binary-name my-app --memory 512 --timeout 30 \
  --env-var RUST_LOG=info --enable-function-url skyzen-api
```

The target triple is named outright rather than left to `cargo lambda`'s default, so the
architecture the manifest declares is the one that gets built. Setting `url = false` passes
`--disable-function-url`, which *removes* a URL an earlier deploy created rather than leaving it
serving.

### Queues

`cargo lambda deploy` uploads a function; it does not subscribe one to a queue. When the manifest
declares a queue consumer, `skyzen deploy --provider aws` says so and prints the command that
finishes the job:

```sh
aws lambda create-event-source-mapping --function-name skyzen-api \
  --event-source-arn <queue-arn> --function-response-types ReportBatchItemFailures
```

`ReportBatchItemFailures` is not optional in practice: without it Lambda ignores the partial batch
response and retries the whole batch whenever any message fails.

The `[[native.queue_consumer]]` polling loops do **not** run inside Lambda. The platform pushes
batches there, and a polling loop inside a function that scales to zero would take messages nothing
is waiting to process.

### Logs

```sh
skyzen logs --provider aws
```

`cargo lambda` has no `logs` subcommand, so this is the AWS CLI tailing the function's log group
(`aws logs tail /aws/lambda/<function> --follow`); trailing arguments are forwarded to it.

### What is unavailable

`WorkerContext` is not attached to a Lambda request. Lambda freezes the execution environment the
moment a response is returned, so post-response work would not run to completion; a handler that
asks for the context is told it is unavailable rather than left with work that silently never runs.

## Azure Functions

### What runs

Skyzen deploys to Functions as a [custom handler][handler]: the Function App runs the compiled
binary as a web server and forwards events to it over HTTP.

- **HTTP triggers** arrive as ordinary HTTP requests, answered by the application's own router. The
  generated `host.json` sets `enableForwardingHttpRequest` and clears `extensions.http.routePrefix`,
  so the path the router sees is the path the client asked for rather than `/api/...`.
- **Queue triggers** arrive as a `POST /{functionName}` carrying the custom handler's JSON envelope.
  The runtime mounts those function names, decodes the envelope — reversing the `skyzen-b64:` /
  `skyzen-utf8:` in-band encoding `AzureStorageQueue` writes — and drives the `#[skyzen::queue]`
  handler with a one-message batch. A handler that fails answers with a non-2xx, which is how a
  custom handler tells the host to redeliver.

The mounted function names take precedence over the application's routes, and *only* while running
under `FUNCTIONS_CUSTOMHANDLER_PORT`. A function whose name is also a literal route the application
serves is a startup error naming both; one that merely overlaps a parameterized route is a warning.

[handler]: https://learn.microsoft.com/azure/azure-functions/functions-custom-handlers

### Configuring

```toml
[azure]
app_name = "skyzen-demo"                 # the Function App to publish to
target = "x86_64-unknown-linux-musl"     # a Function App runs Linux
http_mode = "forward"                    # or "proxy", which streams responses

[[azure.queue_triggers]]
function = "process"                     # also the URL path the host POSTs to
queue = "jobs"                           # the Storage queue name
connection_env = "AzureWebJobsStorage"   # the app setting holding the connection
```

`http_mode = "forward"` buffers the response, which is fine for an API and wrong for server-sent
events or any other stream: pick `"proxy"` (the host's `enableProxyingHttpRequest`) when the
application streams.

### Deploying

```sh
npm install -g azure-functions-core-tools@4 --unsafe-perm true
rustup target add x86_64-unknown-linux-musl
skyzen deploy --provider azure
```

`skyzen build --provider azure` generates the bundle without publishing it, into
`.skyzen/gen/azure/`:

| File | What it does |
| --- | --- |
| `host.json` | Starts the staged binary as the custom handler, forwards HTTP to it, clears the route prefix, and sets `extensions.queues.messageEncoding = "none"` |
| `local.settings.json` | `FUNCTIONS_WORKER_RUNTIME = "Custom"`, for `func start` |
| `http/function.json` | One anonymous catch-all HTTP trigger: route `{*path}`, every method |
| `<function>/function.json` | One queue trigger per `[[azure.queue_triggers]]` entry |
| `<binary>` | The compiled handler, staged beside them |

`messageEncoding = "none"` matters: the queue extension base64-decodes messages by default, which
would corrupt every message Skyzen's own Storage queue client wrote.

The Function App itself is not created by `skyzen`. Create it first — `az functionapp create` with
the .NET / custom handler stack — and set `FUNCTIONS_WORKER_RUNTIME=Custom` in its app settings.

### Cross-compiling

A Function App runs Linux, and `func azure functionapp publish` uploads whatever it is given without
looking, so a handler built on macOS publishes successfully and then fails to start with nothing in
the portal to explain why. `skyzen deploy --provider azure` reads the built binary's magic number
and **refuses to publish** anything that is not an ELF executable, naming the fix:

```toml
# Skyzen.toml
[azure]
target = "x86_64-unknown-linux-musl"
```

```sh
rustup target add x86_64-unknown-linux-musl
brew install filosottile/musl-cross/musl-cross   # macOS; a linker for that target
```

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-musl]
linker = "x86_64-linux-musl-gcc"
```

`skyzen build --provider azure` warns instead of failing, so the bundle can still be inspected from
any machine.

### Logs

```sh
skyzen logs --provider azure
```

Runs `func azure functionapp logstream <app_name>`; trailing arguments are forwarded to it.

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

For Cloudflare, `--dry-run` also prints the internal build step Skyzen would perform before
invoking Wrangler. `deploy --dry-run` is the exception: it does the real work and only skips the
upload, so that it validates something.

## Native Development

```sh
skyzen dev
```

`skyzen dev` runs `cargo run` under a supervisor that restarts the binary on debounced source
changes. Before starting it loads `.env` and then `.env.local` into the child process — never into
its own environment — and refuses to start when a `url_env` / `bucket_env` the manifest names is
set nowhere, naming the manifest key that asked for it. `skyzen new` emits a `.env.example` listing
exactly those variables.

## Shell Completions

```sh
skyzen completions zsh  > "${fpath[1]}/_skyzen"
skyzen completions bash > /etc/bash_completion.d/skyzen
skyzen completions fish > ~/.config/fish/completions/skyzen.fish
```

## Dual-Target Development

A common workflow is to develop natively and deploy to WASM:

1. **Develop** with `skyzen dev` (native) — get fast compile times, logging, and `Ctrl+C`
2. **Test** with `skyzen-test` mocks — no external services needed
3. **Preview** with `skyzen dev --provider cloudflare` — local Workers emulation
4. **Deploy** with `skyzen deploy --provider cloudflare` (or `aws`, or `azure`) — production

The same `#[skyzen::main]` entry point serves every target. Your handler code never changes: what
changes is which environment variables the process finds when it starts, and the runtime reads
those before it binds anything.

### What each platform does with a queue handler

One `#[skyzen::queue]` handler, driven four ways:

| Target | Who runs the loop | Declared as |
| --- | --- | --- |
| Native server | Skyzen polls | `[[native.queue_consumer]]` |
| Cloudflare | The platform pushes | `[[cloudflare.queues.consumers]]` |
| AWS Lambda | The platform pushes | an SQS event source mapping |
| Azure Functions | The host pushes | `[[azure.queue_triggers]]` |

Under either serverless host the polling loops do not run at all — the platform owns delivery
there.
