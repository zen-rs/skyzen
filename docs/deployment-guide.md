# Deploying Skyzen Apps

Skyzen deploys to native servers and Cloudflare Workers. AWS and Azure have service
implementations (`skyzen-aws`, `skyzen-azure`) that a native binary can use, but no deployment
adapter — `--provider` accepts `native` and `cloudflare` only.

## Prerequisites

Run `skyzen doctor` to check your toolchain and your manifest:

```sh
skyzen doctor
skyzen doctor --provider cloudflare
```

It reports, per selected provider:

- that `cargo` (and `wrangler`, for Cloudflare) are on `PATH`;
- that the `wasm32-unknown-unknown` target is installed;
- that `Skyzen.toml` parses, and that every portable `[[service]]` / `[[database]]` has both its
  `[cloudflare.*]` wiring and a matching binding entry;
- that the application's resolved `wasm-bindgen` matches the bindings generator the CLI embeds
  (they share a schema version and must be exactly equal);
- that `wrangler whoami` succeeds, so a deploy will not fail on authentication.

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

### D1 Migrations

SQL migrations live in `migrations/` (wrangler's `migrations_dir` default; override it through
`[cloudflare.raw]`). Apply them to every declared `[[cloudflare.d1_databases]]`:

```sh
skyzen migrate --local     # the emulator's database, for `skyzen dev`
skyzen migrate             # the deployed database
skyzen migrate --env staging
```

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
4. **Deploy** with `skyzen deploy --provider cloudflare` — production

The same `#[skyzen::main]` entry point works for both native and WASM targets. Your handler code never changes.
