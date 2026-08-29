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
reachable through it. A `[[database]]` entry gets a `Db` suffix — `journal` generates `JournalDb` —
because a bare `Journal` reads as a domain type rather than a connection. A handler names the ones
it wants:

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

`backend` selects which keys the rest of the table may hold, and unknown keys are rejected: a
`url_env` under a backend that reads no URL fails the parse rather than being ignored, and a
required key that is missing fails there too rather than at compile time.

| Service Type | Backends |
|--------------|----------|
| `kv` | `redis`, `dynamodb`, `cosmos`, `memory` |
| `storage` | `s3`, `blob`, `memory` |
| `queue` | `sqs`, `servicebus`, `storage-queue`, `memory` |

Every backend below is reachable from any runtime, native or serverless: they are HTTP clients, not
platform bindings. Each one needs its crate — see [Dependencies](#dependencies).

#### `backend = "redis"` (kv)

```toml
[native.service.cache]
backend = "redis"
url_env = "CACHE_URL"      # required: redis://127.0.0.1:6379
```

#### `backend = "dynamodb"` (kv)

```toml
[native.service.sessions]
backend = "dynamodb"
table = "skyzen-sessions"  # required: the table, which must already exist
ttl_attribute = "ttl"      # optional: default "expires_at"
consistent_reads = true    # optional: default false, DynamoDB's own
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `table` | string | **Required.** | The `DynamoDB` table. Its partition key attribute must be `pk` |
| `ttl_attribute` | string | `expires_at` | Attribute expiry timestamps are written to. Enable the table's TTL feature on it |
| `consistent_reads` | bool | `false` | Read with `ConsistentRead`, at twice the read capacity |

No environment variable: the credentials and the region come from the ambient AWS chain
(`AWS_PROFILE`, an instance role, SSO — whatever the SDK finds), exactly as `DynamoKv::from_env`
loads them. A table keyed on an attribute other than `pk` is wired in code with `DynamoKv::new`.

#### `backend = "cosmos"` (kv)

```toml
[native.service.sessions]
backend = "cosmos"
database = "appdb"         # required
container = "sessions"     # required, and read once at startup
```

The account endpoint and key come from `AZURE_COSMOS_ENDPOINT` and `AZURE_COSMOS_KEY`, whose names
`CosmosKv::from_env` fixes — so the wiring names no variable, and `skyzen dev` checks those two by
name. The container is read when the application starts, so a partition key path or a time-to-live
setting this backend cannot work with fails at startup rather than on the first write. An account
reached with an Entra ID credential is wired in code with `CosmosKv::with_credential`.

#### `backend = "s3"` (storage)

```toml
[native.service.uploads]
backend = "s3"
bucket_env = "UPLOADS_BUCKET"   # required: the bucket name, not a URL
```

#### `backend = "blob"` (storage)

```toml
[native.service.uploads]
backend = "blob"
container = "uploads"                          # required
connection_env = "AZURE_STORAGE_CONNECTION_STRING"   # optional: this is the default
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `container` | string | **Required.** | The blob container |
| `connection_env` | string | `AZURE_STORAGE_CONNECTION_STRING` | Variable holding the storage account connection string |

Both forms the portal hands out are read: an account key and a shared access signature. An Azurite
connection string works too, because its `BlobEndpoint` wins over the public-cloud host.

#### `backend = "sqs"` (queue)

```toml
[native.service.jobs]
backend = "sqs"
url_env = "JOBS_QUEUE_URL"   # required: https://sqs.us-east-1.amazonaws.com/123456789012/jobs
```

The URL must name a *standard* queue. A FIFO queue needs a message group id on every send and is
wired in code with `SqsQueue::fifo`, so this backend refuses a `.fifo` URL rather than sending
messages that would be rejected one at a time.

#### `backend = "servicebus"` (queue)

```toml
[native.service.jobs]
backend = "servicebus"
queue = "jobs"                                  # required
connection_env = "SERVICEBUS_CONNECTION_STRING" # optional: this is the default
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `queue` | string | **Required.** | The Service Bus queue |
| `connection_env` | string | `SERVICEBUS_CONNECTION_STRING` | Variable holding the namespace's connection string (`Endpoint=sb://…;SharedAccessKeyName=…;SharedAccessKey=…`) |

#### `backend = "storage-queue"` (queue)

```toml
[native.service.jobs]
backend = "storage-queue"
sas_url_env = "JOBS_SAS_URL"   # required
```

The variable holds the whole signed queue URL, as the portal's "Generate SAS" hands it out:
`https://myaccount.queue.core.windows.net/jobs?sv=2024-11-04&sig=…`. There is no default name,
because the URL *is* the credential: two queues are two unrelated URLs. A queue reached with an
Entra ID credential is wired in code with `AzureStorageQueue::with_credential`.

#### `backend = "memory"` (any type)

```toml
[native.service.cache]
backend = "memory"
```

The in-process mock from `skyzen-test`, for local development and tests. It takes no other key.

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
| `name` | string | **Required.** Logical database name used in wiring sections, and the stem of the generated extractor (`main` generates `MainDb`) |
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
| `backend` | string | **Required.** `postgres`, `mysql`, `sqlite`, `azure-sql` or `rds-data` |
| `url_env` | string | **Required** for `postgres`, `mysql`, `sqlite` and `azure-sql`. Environment variable containing the connection string |

#### `backend = "azure-sql"`

```toml
[native.database.main]
backend = "azure-sql"
url_env = "AZURE_SQL_CONNECTION_STRING"
```

Azure SQL, reached over TDS by `AzureSqlDb`. Azure Database for PostgreSQL and for MySQL are *not*
this backend — they speak the wire protocols sqlx already speaks, so they are `postgres` and
`mysql` above.

What `url_env` names holds the **ADO.NET connection string** the portal hands out
(`Server=tcp:…,1433;Database=…;User ID=…;Password=…;Encrypt=True;`), not a URL. It is read from the
environment for the same reason the URLs are: it carries the password.
`AZURE_SQL_CONNECTION_STRING` is the conventional name, because it is the one `AzureSqlDb::from_env`
reads, but any name works — the wiring passes what it reads to `AzureSqlConfig::new`. `skyzen dev`
refuses to start when the variable is set nowhere, and `skyzen new` lists it in `.env.example`.

`skyzen migrate --provider native` cannot migrate this database: the runner links sqlx, which has
no T-SQL driver. It says so and migrates the others; apply these migrations from the application
itself with `Db::migrate` (see the [Migrations Guide](migrations.md)).

Statements are written with `?` placeholders like everywhere else — `Db` rewrites them to `@P1`,
`@P2`, … and bounds `fetch_one` with `TOP (1)`, because T-SQL has no `LIMIT`.

#### `backend = "rds-data"`

```toml
[native.database.main]
backend = "rds-data"
```

Aurora reached through the RDS Data API — an HTTP service addressed by ARN rather than a server
addressed by URL, which is what lets a Lambda talk to Aurora Serverless with no connection pool.

A Data API call is addressed by four values. Written as above, the table names none of them, and
`RdsDataDb::from_env` reads all four from variables whose names it fixes:

| Variable | Holds |
|----------|-------|
| `RDS_RESOURCE_ARN` | The Aurora cluster's ARN |
| `RDS_SECRET_ARN` | The Secrets Manager secret holding its credentials |
| `RDS_DATABASE` | The database statements run against |
| `RDS_ENGINE` | `aurora-postgresql` or `aurora-mysql` — it decides how placeholders are rewritten |

`skyzen dev` checks all four by name, and `skyzen new` lists them in `.env.example`.

Or the table names all four itself, and nothing is read from the environment — the wiring is built
with `RdsDataDb::from_parts`:

```toml
[native.database.main]
backend = "rds-data"
resource_arn = "arn:aws:rds:us-east-1:111122223333:cluster:skyzen"
secret_arn = "arn:aws:secretsmanager:us-east-1:111122223333:secret:skyzen-Ab12Cd"
database = "appdb"
engine = "aurora-postgresql"
```

| Key | Type | Description |
|-----|------|-------------|
| `resource_arn` | string | The Aurora cluster's ARN |
| `secret_arn` | string | The Secrets Manager secret holding its credentials |
| `database` | string | The database statements run against |
| `engine` | string | `aurora-postgresql` or `aurora-mysql` |

**All four or none.** Naming some and not the rest is a parse error listing the keys that are set
and the ones to add: a wiring that took its ARN from this file and its database from a variable
would be half-declared, and the half nobody wrote down is the half `skyzen dev` cannot check. None
of the four is a secret — the credentials themselves live in Secrets Manager, which is what
`secret_arn` points at — so a checked-in manifest can carry them.

The credentials and the region come from the ambient AWS chain either way, as with `dynamodb`.

`skyzen migrate --provider native` cannot migrate this database: the runner links sqlx and its
drivers, and there is no connection to open. It says so and migrates the others; apply these
migrations from the application itself with `Db::migrate` (see the
[Migrations Guide](migrations.md)).

`skyzen dev` loads `.env` and then `.env.local` into the process it starts, and refuses to start when a variable the manifest names — through a key, or through a backend that fixes the name — is set nowhere. `skyzen new` writes a `.env.example` listing exactly these variables, and `skyzen doctor` warns about the ones that are set nowhere.

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

### Cron Triggers

```toml
[cloudflare.triggers]
crons = ["0 * * * *", "*/15 * * * *"]
```

| Key | Type | Description |
|-----|------|-------------|
| `crons` | string[] | Cron expressions, in Cloudflare's own syntax, that invoke the `scheduled` handler |

Declaring any cron makes the generated Worker shim export a `scheduled` member, which requires a
Rust handler annotated `#[skyzen::scheduled]`. `skyzen dev --provider cloudflare -- --test-scheduled`
fires it on demand rather than waiting for the schedule.

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

## Deploy-time interpolation

String values may contain `${NAME}` placeholders. `skyzen deploy`, `skyzen dev`, `skyzen provision`
and the rest of the CLI expand them from the **process environment of the machine running the
command**, then from `.env` / `.env.local`. Process environment wins, same as native wiring.

This is not the runtime environment of the deployed Worker or Lambda. `url_env = "CACHE_URL"` names
a variable the application reads after it starts. `${CACHE_NAMESPACE_ID}` is replaced **before**
the CLI generates `wrangler.toml` or invokes `cargo lambda`.

```toml
[cloudflare]
name = "api"
compatibility_date = "2025-02-01"
account_id = "${CLOUDFLARE_ACCOUNT_ID}"

[[cloudflare.kv_namespaces]]
binding = "CACHE"
id = "${CACHE_NAMESPACE_ID}"

[cloudflare.vars]
API_URL = "${API_URL}"
```

Put the values in `.env` (gitignored) or export them in the shell that runs the CLI. Skyzen does
not require a per-key mapping in GitHub Actions: write `.env` onto the runner, or export the
variables however the job already does. `CLOUDFLARE_API_TOKEN` is wrangler's own credential and
stays out of the file.

A documented credential form (GitHub PAT, PEM private key, URL with a password, …) stored as a
literal fails the CLI load. A `vars` / `[aws.env]` key whose *name* looks like a secret but whose
value is not a known form is a warning. `${NAME}` is neither. `#[skyzen::main]` does not scan, so
`cargo build` is not a secret gate.

Rules:

- `${NAME}` in string **values** only. Keys are not expanded.
- `NAME` is `[A-Za-z_][A-Za-z0-9_]*`. Hyphens, defaults (`${NAME:-x}`), and unclosed `${` fail the parse.
- A missing name fails the parse, naming the TOML path and the variable.
- `$$` writes a literal `$`. `$${NAME}` is the text `${NAME}`, not a lookup.
- Expansion runs on the parsed document, so a secret containing quotes or newlines cannot break TOML.
- Integers and booleans are not strings: `memory_mb = "${MEM}"` is a type error, not an interpolation.
- `#[skyzen::main]` does **not** expand placeholders. A missing GitHub secret must not fail `cargo build`. Do not wrap `url_env` / `binding` / service names in `${}` — those fields are consumed at compile time as literal names.

Interpolating a secret into `[cloudflare.vars]` or `[aws.env]` still stores it as a plaintext
platform variable. Worker secrets stay on `skyzen secret set`.

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

One capability per backend, plus one per portable extractor:

| Backend | `skyzen add` | Crate |
|---------|--------------|-------|
| `redis` | `redis` | `skyzen-redis` |
| `dynamodb` | `dynamodb` | `skyzen-aws` (`dynamodb`) |
| `cosmos` | `cosmos` | `skyzen-azure` (`cosmos`) |
| `s3` | `s3` | `skyzen-s3` |
| `blob` | `azure-blob` | `skyzen-azure` (`blob`) |
| `sqs` | `sqs` | `skyzen-aws` (`sqs`) |
| `servicebus` | `servicebus` | `skyzen-azure` (`servicebus`) |
| `storage-queue` | `storage-queue` | `skyzen-azure` (`storage-queue`) |
| `postgres` / `mysql` / `sqlite` | same name | `skyzen-services` (that feature) |
| `azure-sql` | `azure-sql` | `skyzen-azure` (`sql`) |
| `rds-data` | `rds-data` | `skyzen-aws` (`rds-data`) |
| `memory` | `memory` | `skyzen-test` |

`skyzen add --list` prints them all.

`skyzen add` shells out to `cargo add`, so the versions are whatever the registry actually has.
