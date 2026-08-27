# Skyzen.toml Reference

`Skyzen.toml` is an optional manifest file that declares portable capabilities plus platform-specific wiring and deployment configuration. It is used by `#[skyzen::main]` for generated wiring and by the `skyzen` CLI for local emulation (`skyzen dev`) and deployment (`skyzen deploy`).

Users can always wire services manually in Rust without using `Skyzen.toml`.

Both consumers parse the file through the same schema crate (`skyzen-manifest`), so a section can never be accepted at compile time and rejected at deploy time. Every table rejects unknown keys and every `type` / `backend` value is checked against a closed set, so a typo fails immediately instead of silently dropping a binding.

> **Migrating from `[[datasource]]`.** The legacy `[[datasource]]` section has been removed. Declare a `[[database]]` with `[native.database.<name>]` wiring instead — it generates the same kind of typed extractor and works on Cloudflare D1 as well as native SQL.

## Portable Services

Declare logical portable services once:

```toml
[[service]]
name = "cache"
type = "kv"

[[service]]
name = "uploads"
type = "storage"

[[service]]
name = "jobs"
type = "queue"
```

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | **Required.** Logical service name used in wiring sections, and the name of the generated extractor type |
| `type` | string | **Required.** `kv`, `storage`, or `queue` |

### Multiple Instances of One Type

Declare as many services of a type as you need — multiple KV namespaces and multiple R2 buckets
are routine in a single Worker, and so are multiple caches and buckets on AWS:

```toml
[[service]]
name = "cache"
type = "kv"

[[service]]
name = "sessions"
type = "kv"
```

Each entry generates its own extractor type, named after the entry, wrapping the portable service:
`cache` generates `pub struct Cache(Kv)` with `Deref<Target = Kv>`, so every `Kv` method is
reachable through it. A handler names the ones it wants:

```rust
async fn handler(cache: Cache, sessions: Sessions) -> Result<&'static str> {
    cache.put("greeting", b"hello").await?;
    sessions.put_if_absent("sid", b"{}").await?;
    Ok("ok")
}
```

**How a bare `Kv` / `Storage` / `Queue` resolves.** Services reach handlers through request
extensions, which are keyed by type, so a bare wrapper can only ever name one instance.
`#[skyzen::main]` injects it only when the manifest declares **exactly one** service of that type.
With the two KV entries above, only `Cache` and `Sessions` are injected, and a handler asking for a
bare `Kv` gets its `KvNotConfigured` error (HTTP 500) rather than one of the two chosen
arbitrarily — name the binding you mean.

The same rule applies to `Db`, except that `[[database]]` picks its bare instance explicitly with
`default = true`.

### Native Service Wiring

Wire each declared service for native targets:

```toml
[native.service.cache]
backend = "redis"
url_env = "CACHE_URL"

[native.service.uploads]
backend = "s3"
bucket_env = "UPLOADS_BUCKET"

[native.service.jobs]
backend = "sqs"
url_env = "JOBS_QUEUE_URL"
```

Supported native backends:

| Service Type | Backends | Required Keys |
|--------------|----------|---------------|
| `kv` | `redis`, `memory` | `url_env` for `redis` |
| `storage` | `s3`, `memory` | `bucket_env` for `s3` |
| `queue` | `sqs`, `memory` | `url_env` for `sqs` |

### Native Queue Consumers

Cloudflare pushes queue batches into a Worker. Natively nobody does, so declaring a consumer is
what makes Skyzen run the polling loop itself — receive, call the `#[skyzen::queue]` handler,
settle — beside the HTTP server:

```toml
[[native.queue_consumer]]
service = "jobs"
concurrency = 4
batch_size = 10
poll_wait = "20s"
visibility_timeout = "60s"
retry_delay = "30s"
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `service` | string | **Required.** | The `[[service]]` to consume. It must be `type = "queue"` |
| `concurrency` | integer | `1` | Polling loops run against this queue at once. Must be at least 1 |
| `batch_size` | integer | `10` | Most messages per receive. Backends cap it further (SQS 10, Azure Storage 32) |
| `poll_wait` | duration | `"20s"` | How long a receive waits for a message, and the loop's idle pace |
| `visibility_timeout` | duration | queue default | How long a received batch stays invisible to other consumers |
| `retry_delay` | duration | `"30s"` | Redelivery delay for a retry the handler did not delay itself. Whole seconds only |

Durations are humantime strings (`"20s"`, `"1m 30s"`, `"500ms"`) and are parsed when the manifest
is read, so a malformed one fails the build rather than the consumer.

Declaring a consumer requires a `#[skyzen::queue]` handler in the same module as
`#[skyzen::main]`, taking exactly one `QueueBatch<T>` argument; a handler that nothing consumes —
no `[[native.queue_consumer]]` and no `[[cloudflare.queues.consumers]]` — is a compile error
rather than dead code. See the [services guide](services-guide.md) for the delivery semantics.

### Cloudflare Service Wiring

Wire each declared service for Cloudflare/WASM targets:

```toml
[cloudflare.service.cache]
binding = "CACHE"

[cloudflare.service.uploads]
binding = "UPLOADS"

[cloudflare.service.jobs]
binding = "JOBS"
```

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Cloudflare binding name used to initialize the provider backend |

## Portable Databases

Declare a logical portable SQL database:

```toml
[[database]]
name = "main"
type = "sql"
```

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | **Required.** Logical database name used in wiring sections |
| `type` | string | **Required.** Currently only `sql` |
| `default` | bool | Whether a bare `Db` extractor resolves to this entry. Required (on exactly one entry) when more than one database is declared |
| `migrations_dir` | string | Directory of `<version>_<name>.sql` migrations, relative to the project root. Defaults to `migrations` |

`migrations_dir` defaults to `migrations/`, which is also `wrangler d1 migrations apply`'s default,
so a database deployed to D1 and one run natively read the same files with nothing configured.
Changing it moves only the native path — wrangler needs its own `migrations_dir` under
`[cloudflare.raw]`. See the [Migrations Guide](migrations.md).

### Native Database Wiring

```toml
[native.database.main]
backend = "postgres"
url_env = "DATABASE_URL"
```

| Key | Type | Description |
|-----|------|-------------|
| `backend` | string | **Required.** `postgres`, `mysql` or `sqlite` |
| `url_env` | string | **Required.** Environment variable containing the connection URL |

`skyzen dev` loads `.env` and then `.env.local` into the process it starts, and refuses to start when a `url_env` / `bucket_env` the manifest names is set nowhere. `skyzen new` writes a `.env.example` listing exactly these variables.

### Cloudflare Database Wiring

```toml
[cloudflare.database.main]
binding = "DB"
```

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Cloudflare D1 binding name |

## Cloudflare Section

```toml
[cloudflare]
name = "my-worker"
main = "dist/worker.js"
compatibility_date = "2025-02-01"
compatibility_flags = ["nodejs_compat"]
account_id = "abc123"
workers_dev = true
route = "example.com/*"
zone_id = "zone123"

[cloudflare.vars]
API_URL = "https://api.example.com"
```

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | Worker name |
| `main` | string | Worker entry path relative to the project root. Default is `dist/worker.js`. Skyzen generates this file automatically for Cloudflare dev/deploy. |
| `compatibility_date` | string | Workers compatibility date |
| `compatibility_flags` | string[] | Compatibility flags (e.g. `["nodejs_compat"]`) |
| `account_id` | string | Cloudflare account ID |
| `workers_dev` | bool | Enable `*.workers.dev` subdomain |
| `route` | string | URL pattern for routing |
| `zone_id` | string | Cloudflare zone ID |
| `vars` | table | Plaintext environment variables. Use `skyzen secret set` for anything sensitive |

> **Two different `service` keys.** `[cloudflare.service.<name>]` (a table, below) wires a **portable** `[[service]]` entry to a Cloudflare binding. `[[cloudflare.services]]` (an array, below) declares a wrangler **service binding** to another Worker. They are unrelated.

### KV Namespaces

```toml
[[cloudflare.kv_namespaces]]
binding = "MY_KV"
id = "abc123"
preview_id = "def456"
```

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Binding name used in `CfKv::from_env(&env, "MY_KV")` |
| `id` | string | KV namespace ID. Leave it out and run `skyzen provision --provider cloudflare` to have one created and written back |
| `preview_id` | string | Optional. Preview namespace ID for `wrangler dev --remote` |

A binding with no `id` binds a local namespace under `skyzen dev` (which never contacts Cloudflare) and is refused by `skyzen deploy`, which names `skyzen provision`.

### R2 Buckets

```toml
[[cloudflare.r2_buckets]]
binding = "MY_BUCKET"
bucket_name = "my-bucket"
preview_bucket_name = "my-bucket-preview"
```

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Binding name used in `CfR2::from_env(&env, "MY_BUCKET")` |
| `bucket_name` | string | **Required.** R2 bucket name |
| `preview_bucket_name` | string | Optional. Preview bucket name |

### D1 Databases

```toml
[[cloudflare.d1_databases]]
binding = "DB"
database_name = "app"
database_id = "your-d1-id"
preview_database_id = "preview-d1-id"
```

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Binding name used in `CfD1::from_env(&env, "DB")` |
| `database_name` | string | **Required.** D1 database name, and the name `skyzen provision` creates it under |
| `database_id` | string | D1 database ID. Leave it out and run `skyzen provision --provider cloudflare` |
| `preview_database_id` | string | Optional. Preview database ID |

### Queues

```toml
[[cloudflare.queues.producers]]
binding = "MY_QUEUE"
queue = "my-queue"
delivery_delay = 30

[[cloudflare.queues.consumers]]
queue = "my-queue"
max_batch_size = 10
max_batch_timeout = 5
max_retries = 3
dead_letter_queue = "my-queue-dlq"
max_concurrency = 4
retry_delay = 60
```

**Producers:**

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Binding name used in `CfQueue::from_env(&env, "MY_QUEUE")` |
| `queue` | string | **Required.** Queue name |
| `delivery_delay` | integer | Seconds to hold every message before consumers can see it |

**Consumers:**

| Key | Type | Description |
|-----|------|-------------|
| `queue` | string | **Required.** Queue name to consume from |
| `max_batch_size` | integer | Most messages delivered to one `queue` invocation |
| `max_batch_timeout` | integer | Seconds to wait for a batch to fill before delivering a partial one |
| `max_retries` | integer | Times a message is retried before it is dropped or dead-lettered |
| `dead_letter_queue` | string | Queue exhausted messages are forwarded to |
| `max_concurrency` | integer | Most consumer invocations Cloudflare runs at once |
| `retry_delay` | integer | Seconds to hold a retried message before redelivering it |

Declaring a consumer makes the generated Worker shim export a `queue` member, which requires a Rust handler annotated `#[skyzen::queue]`.

### Service Bindings

Bind another Worker so this one can call it directly:

```toml
[[cloudflare.services]]
binding = "AUTH"
service = "auth-worker"
environment = "production"
```

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Binding name, as seen from `env` |
| `service` | string | **Required.** The name of the Worker being bound |
| `environment` | string | Optional. That Worker's named environment |

### Static Assets

```toml
[cloudflare.assets]
directory = "public"
binding = "ASSETS"
not_found_handling = "single-page-application"
run_worker_first = false
```

| Key | Type | Description |
|-----|------|-------------|
| `directory` | string | **Required.** Directory of files to upload, relative to the project root |
| `binding` | string | Optional. Binding for reading assets programmatically from the Worker |
| `not_found_handling` | string | Optional. `none`, `404-page` or `single-page-application` |
| `run_worker_first` | bool | Optional. Run the Worker before the asset server rather than after it |

### Secrets Store

```toml
[[cloudflare.secrets_store_secrets]]
binding = "API_KEY"
store_id = "your-store-id"
secret_name = "api-key"
```

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Binding name, as seen from `env` |
| `store_id` | string | **Required.** The secrets store the secret lives in |
| `secret_name` | string | **Required.** The secret's name within that store |

### Handlers

```toml
[cloudflare.handlers]
email = true
tail = true
```

| Key | Type | Description |
|-----|------|-------------|
| `email` | bool | Export an `email` handler, implemented in Rust with `#[skyzen::email]` |
| `tail` | bool | Export a `tail` handler, implemented in Rust with `#[skyzen::tail]` |

`queue` and `scheduled` handlers are inferred from `[[cloudflare.queues.consumers]]` and `[cloudflare.triggers]`. Email routing and tail consumers are configured on the *sending* side — in the dashboard, or in another Worker's `tail_consumers` — so this Worker's manifest has nothing to infer them from, and they are opted into explicitly.

### Durable Objects

```toml
[[cloudflare.durable_objects.bindings]]
name = "STATE"
class_name = "State"
script_name = "other-worker"

[[cloudflare.durable_objects.migrations]]
tag = "v1"
new_sqlite_classes = ["State"]

[[cloudflare.durable_objects.migrations]]
tag = "v2"
new_classes = ["Counter"]

[[cloudflare.durable_objects.migrations]]
tag = "v3"
deleted_classes = ["OldClass"]

[[cloudflare.durable_objects.migrations]]
tag = "v4"
renamed_classes = [{ from = "Old", to = "New" }]
```

**Bindings:**

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | **Required.** Binding name |
| `class_name` | string | **Required.** Durable Object class name |
| `script_name` | string | Optional. Script name if DO is defined in another Worker |

**Migrations:**

| Key | Type | Description |
|-----|------|-------------|
| `tag` | string | **Required.** Migration version tag (e.g. `"v1"`, `"v2"`) — bump whenever class definitions change |
| `new_classes` | string[] | Classes to create (standard storage) |
| `new_sqlite_classes` | string[] | Classes to create (SQLite-backed Durable Object storage) |
| `deleted_classes` | string[] | Classes to remove |
| `renamed_classes` | object[] | Classes to rename (`{ from = "Old", to = "New" }`) |

## AWS Section

`[aws]` configures an AWS Lambda deployment. Nothing here reaches the application at compile time:
the same binary serves every target, so this is only what `cargo lambda` needs to be told.

```toml
[aws]
function_name = "skyzen-api"   # optional; defaults to the binary's own name
memory_mb = 512                # optional; Lambda scales CPU with memory
timeout = "30s"                # optional humantime; sent to Lambda as whole seconds
architecture = "arm64"         # "arm64" (default) or "x86_64"
url = true                     # create and keep a Function URL; default true

[aws.env]                      # plaintext environment variables set on the function
RUST_LOG = "info"
```

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `function_name` | string | the binary's name | Passed positionally to `cargo lambda deploy` |
| `memory_mb` | integer > 0 | Lambda's own default | `--memory` |
| `timeout` | humantime duration | Lambda's own default | `--timeout`, in seconds |
| `architecture` | `"arm64"` \| `"x86_64"` | `"arm64"` | Selects the build's target triple |
| `url` | boolean | `true` | `false` passes `--disable-function-url`, removing an existing URL |
| `env` | table of strings | empty | One `--env-var` each; not for secrets |

`url` defaults to `true` because a Skyzen application is an HTTP server: without a Function URL — or
an API Gateway wired up by hand — nothing can reach it.

An SQS-triggered Lambda additionally needs an event source mapping, which `cargo lambda deploy`
does not create. See the [deployment guide](deployment-guide.md#queues).

## Azure Section

`[azure]` configures an Azure Functions deployment, where the binary runs as a custom handler.

```toml
[azure]
app_name = "skyzen-demo"                 # the Function App to publish to
target = "x86_64-unknown-linux-musl"     # a Function App runs Linux
http_mode = "forward"                    # "forward" (default) or "proxy"

[[azure.queue_triggers]]
function = "process"                     # the Functions name, and the path the host POSTs to
queue = "jobs"                           # the Storage queue to trigger on
connection_env = "AzureWebJobsStorage"   # the app setting holding the connection
```

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `app_name` | string | none | Required by `deploy` and `logs`; nothing can infer it |
| `target` | string | the host's own target | A Linux triple; a macOS build is refused before publishing |
| `http_mode` | `"forward"` \| `"proxy"` | `"forward"` | `proxy` streams responses; `forward` buffers them |
| `queue_triggers` | array of tables | empty | One Functions queue trigger each |

Each `[[azure.queue_triggers]]` entry needs all three keys. `function` must be a valid Functions
name (a letter, then letters, digits, hyphens or underscores) and each one must be unique — both are
compile-time errors, because the name is also a URL path the runtime mounts.

A `function` whose path the application already serves as a literal route is a **startup** error:
under the Functions host the trigger takes precedence, so the route would silently stop being
reachable.

## Environments

Environment overlays are Cloudflare-only: `[cloudflare.env.<name>]` exists because wrangler models
environments itself, and neither `cargo lambda` nor the Functions host does.

`[cloudflare.env.<name>]` declares an overlay with the same shape as `[cloudflare]`, selected with
`skyzen <command> --env <name>` and forwarded to wrangler as `--env <name>`:

```toml
[cloudflare]
name = "my-app"
compatibility_date = "2025-02-01"
workers_dev = true

[[cloudflare.kv_namespaces]]
binding = "CACHE"
id = "production-namespace-id"

[cloudflare.env.staging]
workers_dev = false

[[cloudflare.env.staging.kv_namespaces]]
binding = "CACHE"
id = "staging-namespace-id"
```

**Merge semantics.** An overlay is merged into the base table before anything is interpreted:

- A key holding a **table** on both sides is merged key by key, recursively.
- Anything else — including **arrays** — is **replaced** wholesale. An overlay that changes one KV namespace has to restate the whole `kv_namespaces` list, because an array has no element identity to merge on.
- A key present only in the base survives untouched.

Every declared environment is resolved and validated when the manifest is read, so a typo in an environment nobody selected still fails the parse. Selecting an environment the manifest does not declare is an error, never a silent fall back to the base.

The generated `wrangler.toml` contains a complete `[env.<name>]` section for each overlay rather than only the differences, because wrangler's named environments do **not** inherit bindings. An environment's Worker name is the base name suffixed with the environment (`my-app-staging`) unless the overlay sets its own `name`.

`[cloudflare.env.<name>.env]` is rejected: overlays do not nest.

## Escape Hatch

`[cloudflare.raw]` is merged verbatim into the generated `wrangler.toml`, using the same rules as an
environment overlay. Anything wrangler accepts but Skyzen does not model goes here — and because a
scalar replaces, it can also override what Skyzen rendered:

```toml
[cloudflare.raw]
workers_dev = false

[cloudflare.raw.observability]
enabled = true

[[cloudflare.raw.vectorize]]
binding = "VEC"
index_name = "docs"
```

Keys under `raw` are not validated by Skyzen; wrangler is the one that rejects them.

## Full Example

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

[cloudflare]
name = "my-app"
main = "dist/worker.js"
compatibility_date = "2025-02-01"
workers_dev = true

[cloudflare.service.cache]
binding = "CACHE"

[cloudflare.service.uploads]
binding = "UPLOADS"

[cloudflare.database.main]
binding = "DB"

[[cloudflare.kv_namespaces]]
binding = "CACHE"
id = "abc123"

[[cloudflare.r2_buckets]]
binding = "UPLOADS"
bucket_name = "my-uploads"

[[cloudflare.d1_databases]]
binding = "DB"
database_name = "app"
database_id = "d1-id-here"

[[cloudflare.durable_objects.bindings]]
name = "STATE"
class_name = "AppState"

[[cloudflare.durable_objects.migrations]]
tag = "v1"
new_sqlite_classes = ["AppState"]
```

## Provider Mapping

The `skyzen` CLI generates provider-specific config files from `Skyzen.toml`:

| Provider | Generated Config | `dev` Command | `deploy` Command |
|----------|-----------------|---------------|-----------------|
| Cloudflare | `.skyzen/gen/wrangler.toml`, `dist/worker.js`, `dist/worker_bg.js`, `dist/worker_bg.wasm` | `wrangler dev --local` | `wrangler deploy` |
| AWS | none — the flags are derived from `[aws]` | — (run it as a server with `skyzen dev`) | `cargo lambda build` then `cargo lambda deploy` |
| Azure | `.skyzen/gen/azure/{host.json, local.settings.json, <function>/function.json}` plus the staged binary | — (`func start` over a built bundle) | `func azure functionapp publish` |

The generated files are derived artifacts, rewritten on every run — edit `Skyzen.toml`, not them.

Run `skyzen doctor` to check the toolchain, the manifest, and that every portable capability has a
matching binding.

## Dependencies

Declaring a capability does not add the crate that implements it. `skyzen dev` and `skyzen deploy`
check the project's `Cargo.toml` and fail with the exact command when one is missing:

```sh
skyzen add redis        # backend = "redis"
skyzen add cloudflare   # any [cloudflare.service.*] / [cloudflare.database.*] wiring
skyzen add --list kv    # print the `cargo add` invocations without running them
```

`skyzen add` shells out to `cargo add`, so the versions are whatever the registry actually has.
