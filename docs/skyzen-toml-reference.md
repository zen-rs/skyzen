# Skyzen.toml Reference

`Skyzen.toml` is an optional manifest file that declares datasources and platform-specific deployment configuration. It is used by the `skyzen` CLI for local emulation (`skyzen dev`) and deployment (`skyzen deploy`).

Users can always wire services manually in Rust without using `Skyzen.toml`.

## Datasources

Declare datasources that `import_config!()` will generate typed code for:

```toml
[[datasource]]
name = "MainDb"
engine = "postgres"
strategy = "tcp"
url_from_env = "DATABASE_URL"
key_from_env = "DATABASE_TOKEN"
```

| Key | Type | Description |
|-----|------|-------------|
| `name` | string | **Required.** Name for the generated type (e.g. `"MainDb"` generates a `MainDb` extractor) |
| `engine` | string | **Required.** Database engine: `"postgres"`, `"mysql"`, `"sqlite"` |
| `strategy` | string | **Required.** Connection strategy: `"tcp"` |
| `url_from_env` | string | **Required.** Environment variable containing the connection URL |
| `key_from_env` | string | Optional. Environment variable containing an auth token or key |

When you use `#[skyzen::main]`, datasources are automatically initialized and injected as middleware. Without `#[skyzen::main]`, call `import_config!()` and wire middleware yourself.

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
| `main` | string | Entry point path |
| `compatibility_date` | string | Workers compatibility date |
| `compatibility_flags` | string[] | Compatibility flags (e.g. `["nodejs_compat"]`) |
| `account_id` | string | Cloudflare account ID |
| `workers_dev` | bool | Enable `*.workers.dev` subdomain |
| `route` | string | URL pattern for routing |
| `zone_id` | string | Cloudflare zone ID |
| `vars` | table | Environment variables |

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
| `id` | string | **Required.** KV namespace ID |
| `preview_id` | string | Optional. Preview namespace ID for `wrangler dev` |

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
| `database_name` | string | **Required.** D1 database name |
| `database_id` | string | **Required.** D1 database ID |
| `preview_database_id` | string | Optional. Preview database ID |

### Queues

```toml
[[cloudflare.queues.producers]]
binding = "MY_QUEUE"
queue = "my-queue"

[[cloudflare.queues.consumers]]
queue = "my-queue"
```

**Producers:**

| Key | Type | Description |
|-----|------|-------------|
| `binding` | string | **Required.** Binding name used in `CfQueue::from_env(&env, "MY_QUEUE")` |
| `queue` | string | **Required.** Queue name |

**Consumers:**

| Key | Type | Description |
|-----|------|-------------|
| `queue` | string | **Required.** Queue name to consume from |

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
| `new_sqlite_classes` | string[] | Classes to create (SQLite storage, for `CfDurableSqlite`) |
| `deleted_classes` | string[] | Classes to remove |
| `renamed_classes` | object[] | Classes to rename (`{ from = "Old", to = "New" }`) |

## AWS Section

```toml
[aws]
template = "template.yaml"
stack_name = "my-stack"
region = "us-east-1"
profile = "my-profile"
local_port = 3001
env_vars = ".env"
```

| Key | Type | Description |
|-----|------|-------------|
| `template` | string | SAM/CloudFormation template path |
| `stack_name` | string | CloudFormation stack name |
| `region` | string | AWS region |
| `profile` | string | AWS CLI profile name |
| `local_port` | u16 | Port for `sam local start-api` |
| `env_vars` | string | Path to env vars file for local development |

The `skyzen` CLI runs:
- `skyzen dev --provider aws` → `sam local start-api`
- `skyzen deploy --provider aws` → `sam deploy`

## Azure Section

```toml
[azure]
project = "."
app_name = "my-function-app"
port = 7071
```

| Key | Type | Description |
|-----|------|-------------|
| `project` | string | Azure Functions project directory |
| `app_name` | string | Function app name for deployment |
| `port` | u16 | Port for `func start` |

The `skyzen` CLI runs:
- `skyzen dev --provider azure` → `func start`
- `skyzen deploy --provider azure` → `func azure functionapp publish`

## Full Example

```toml
[[datasource]]
name = "MainDb"
engine = "postgres"
strategy = "tcp"
url_from_env = "DATABASE_URL"

[cloudflare]
name = "my-app"
main = "dist/worker.js"
compatibility_date = "2025-02-01"
workers_dev = true

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

[aws]
template = "template.yaml"
stack_name = "my-app-stack"
region = "us-east-1"

[azure]
project = "."
app_name = "my-app"
port = 7071
```

## Provider Mapping

The `skyzen` CLI generates provider-specific config files from `Skyzen.toml`:

| Provider | Generated Config | `dev` Command | `deploy` Command |
|----------|-----------------|---------------|-----------------|
| Cloudflare | `.skyzen/gen/wrangler.toml` | `wrangler dev` | `wrangler deploy` |
| AWS | uses `template` directly | `sam local start-api` | `sam deploy` |
| Azure | uses `project` directly | `func start` | `func azure functionapp publish` |

Run `skyzen doctor` to verify that required provider tools are installed.
