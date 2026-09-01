# skyzen-azure

[![crates.io](https://img.shields.io/crates/v/skyzen-azure.svg)](https://crates.io/crates/skyzen-azure)
[![docs.rs](https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square)](https://docs.rs/skyzen-azure)
[![License](https://img.shields.io/crates/l/skyzen-azure.svg)](../LICENSE)

Azure service implementations for the Skyzen framework.

## Overview

`skyzen-azure` provides implementations of Skyzen's service traits for Azure cloud services:

| Type | Implements | Azure Service |
|------|-----------|---------------|
| `CosmosKv` | `KeyValueStore` | [Azure Cosmos DB](https://learn.microsoft.com/azure/cosmos-db/) |
| `AzureBlob` | `ObjectStorage` | [Azure Blob Storage](https://learn.microsoft.com/azure/storage/blobs/) |
| `ServiceBusQueue` | `MessageQueue` | [Azure Service Bus](https://learn.microsoft.com/azure/service-bus-messaging/) |
| `AzureStorageQueue` | `MessageQueue` | [Azure Storage queues](https://learn.microsoft.com/azure/storage/queues/) |
| `AzureSqlDb` | `DbBackend` | [Azure SQL](https://learn.microsoft.com/azure/azure-sql/) |

`AzureSqlDb` is only for **Azure SQL**, which speaks T-SQL over TDS. Azure Database for PostgreSQL
and Azure Database for MySQL speak the wire protocols sqlx already speaks, so they need nothing from
this crate — `Db::connect_postgres` and `Db::connect_mysql` reach them directly.

## Installation

```toml
[dependencies]
skyzen-azure = "0.1"
```

### Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `cosmos` | Yes | Cosmos DB `KeyValueStore` via `azure_data_cosmos` |
| `blob` | Yes | Blob Storage `ObjectStorage` via Apache OpenDAL, plus the REST calls OpenDAL does not cover |
| `servicebus` | Yes | Service Bus `MessageQueue`, speaking the Service Bus REST API on `reqwest` |
| `storage-queue` | Yes | Azure Storage queue `MessageQueue` via `azure_storage_queue` |
| `sql` | Yes | Azure SQL `DbBackend` via `tiberius` behind a `deadpool` pool |

Disable unused features to reduce compile times:

```toml
skyzen-azure = { version = "0.1", default-features = false, features = ["cosmos", "blob"] }
```

## Usage

### CosmosKv

Each key is one document: the key is its `id`, the value its base64-encoded `value`. Building reads
the container's own partition key definition and fills whatever path it names, so the store binds to
a container that already exists rather than requiring one shaped a particular way.

```rust
use skyzen_azure::{CosmosKv, PartitionStrategy};
use skyzen_services::Kv;

// AZURE_COSMOS_ENDPOINT + AZURE_COSMOS_KEY
let cosmos = CosmosKv::from_env("app", "kv").await?;
let kv = Kv::new(cosmos);

kv.put_json("user:1", &user).await?;
let user: Option<User> = kv.get_json("user:1").await?;

// Or bind an existing container, keeping every key of this store in one partition:
let cosmos = CosmosKv::builder(container_client)
    .with_partition_strategy(PartitionStrategy::Fixed("acme".to_owned()))
    .build()
    .await?;
```

`put_with_ttl` and `expire` need the container to have time-to-live enabled; binding to one that
does not reports `Unsupported` rather than storing a value that would silently never expire.
`list` carries Cosmos' own continuation token, so resume a listing with the prefix it started with.

### AzureBlob

```rust
use skyzen_azure::{AzureBlob, AzureBlobAuth, AzureBlobConfig};
use skyzen_services::Storage;

// AZURE_STORAGE_CONNECTION_STRING
let blob = AzureBlob::from_env("uploads")?;
let storage = Storage::new(blob);

storage.put("images/photo.png", image_bytes).await?;
let object = storage.get("images/photo.png").await?;

// Or assemble the configuration by hand — an Azurite container, for instance:
let blob = AzureBlob::new(
    AzureBlobConfig::new("devstoreaccount1", "uploads", AzureBlobAuth::AccountKey(key))
        .with_endpoint("http://127.0.0.1:10000/devstoreaccount1"),
)?;
```

Listing pages through Azure's own `marker`, so a full walk costs one request per page rather than
re-reading what it has already returned. Presigned URLs need `AzureBlobAuth::AccountKey`: a shared
access signature cannot mint a narrower one, and asking it to reports `Unsupported`.

`put_with` records the content type, custom metadata, `Cache-Control` and the storage tier; the
options OpenDAL's azblob writer has no header for (`Content-Encoding`, `Content-Disposition`,
`Content-MD5`) are refused rather than dropped. A presigned upload carries all of them, because the
client builds that request itself.

### ServiceBusQueue

```rust
use skyzen_azure::ServiceBusQueue;
use skyzen_services::{queue::ReceiveOptions, Queue};

// SERVICEBUS_CONNECTION_STRING
let queue = Queue::new(ServiceBusQueue::from_env("jobs")?);

queue.send_json(&event).await?;

let options = ReceiveOptions::new()
    .with_max_messages(10)
    .with_wait(Duration::from_secs(30));

for message in queue.receive(options).await? {
    queue.ack(&message.receipt).await?;
}
```

UTF-8 payloads travel verbatim; binary ones are base64-encoded and tagged with a
`skyzen-content-encoding` message property, so the wire format is unambiguous either way.
Consumption is peek-lock, and the lease lasts the queue's configured `LockDuration` — the REST API
has no per-delivery visibility timeout, so asking for one reports `Unsupported`. A peek-lock always
waits for a message, and the REST API documents no non-blocking form, so `receive` needs an explicit
`wait` rather than blocking for the service's own default and calling it immediate. `send_batch` is a
request per message (the REST API has no batch send) and reports `PartialBatch` naming exactly which
entries were rejected.

### AzureStorageQueue

The simpler, cheaper Azure queue: flat, text-bodied, backed by a storage account.

```rust
use skyzen_azure::AzureStorageQueue;
use skyzen_services::Queue;

let queue = Queue::new(AzureStorageQueue::from_sas_url(&sas_url)?);
queue.send_json(&event).await?;
```

A message body travels as text inside an XML document, so bodies XML cannot carry are base64-encoded
behind a `skyzen-b64:` prefix and text that would look like a prefix is escaped behind
`skyzen-utf8:`. That encoding is `skyzen_services::queue::envelope`, not this crate's own: the
framework's Azure Functions integration has to reverse it for messages the host delivers, and a
platform crate never depends on the framework crate.

Messages are capped at 64 KB of encoded text and one `receive` takes at most 32. The service has no
long polling of its own, so `ReceiveOptions::wait` is **emulated** — the queue is re-polled every
`POLL_INTERVAL` until a message arrives or the wait elapses, which is what lets one portable
consumer loop drive this backend and a genuinely long-polling one with the same options.

### AzureSqlDb

The portable `Db` API over Azure SQL. Handlers write `?` placeholders and plain `SELECT`s, exactly
as they do against Postgres or D1.

```rust
use skyzen_azure::{AzureSqlConfig, AzureSqlDb};
use skyzen_services::Db;

// AZURE_SQL_CONNECTION_STRING holds the portal's ADO.NET connection string.
let db = Db::new(AzureSqlDb::from_env()?);

let user: User = db
    .query("SELECT [id], [name] FROM [users] WHERE [id] = ?")
    .bind(7_i64)
    .fetch_one()
    .await?;

// Or configure it explicitly, capping the pool:
let db = Db::new(AzureSqlDb::new(
    AzureSqlConfig::new(connection_string).with_max_pool_size(4),
)?);
```

**Declaratively.** The same backend is wired from `Skyzen.toml` with

```toml
[native.database.main]
backend = "azure-sql"
url_env = "AZURE_SQL_CONNECTION_STRING"
```

where `url_env` names the variable holding the connection string — `skyzen dev` refuses to start
without it, and `skyzen add azure-sql` installs this crate with the `sql` feature. See the
[Skyzen.toml reference](../docs/skyzen-toml-reference.md#native-database-wiring).

**Dialect.** `dialect()` is `DbDialect::Mssql`. Skyzen rewrites each `?` into `@P1`, `@P2`, … before
execution, and bounds `fetch_one`/`fetch_optional` with `TOP (1)` rather than `LIMIT 1` — T-SQL has
no `LIMIT`. Writing `@P1` yourself collides with the generated name, the same way a hand-written
`$1` does on Postgres; bind with `?`.

Because `TOP` bounds the query it sits inside rather than the whole statement, the rewrite steps
aside for a `WITH` query and for `UNION` / `EXCEPT` / `INTERSECT`, which cost the optimization and
never correctness.

**Connection string.** The ADO.NET form the portal hands out is what `from_env` and `AzureSqlConfig`
take. Two things are read out of it before tiberius sees it:

- if it does not mention `Encrypt`, encryption is set to **required** — tiberius would otherwise
  default it off and Azure SQL would refuse the login with a handshake error naming nothing;
- `Authentication=` is **refused** unless it says `SqlPassword`. tiberius supports SQL Server
  authentication only, and its parser ignores the keyword, so a Microsoft Entra ID value would
  silently fall through to an empty username and fail as `Login failed for user ''`.

Nothing dials at construction: the pool connects lazily, so a wrong password surfaces on the first
query rather than at `from_env()`.

**Transactions.** `begin()` is a real interactive transaction. It takes one connection out of the
pool and keeps it — `BEGIN TRANSACTION` is session state, so a transaction whose statements landed
on different pooled connections would commit nothing. A clean commit or rollback returns the
connection; a commit or rollback that *fails*, or a transaction dropped without either, closes it
instead of handing back a connection that may still be inside a transaction (the drop case logs an
error — call `commit()` or `rollback()`). The rollback is `IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION`,
because SQL Server rolls a deadlock victim back itself and a bare `ROLLBACK` would then fail with
"no corresponding BEGIN TRANSACTION" and hide the deadlock. `execute_batch` runs its statements in
one such transaction, rolling back on the first failure.

**Types.** `DbValue::Timestamp` binds as `datetimeoffset` at `+00:00`, `Uuid` as
`uniqueidentifier`, `Decimal` as `numeric`, and `Json` as `nvarchar` — SQL Server has no JSON column
type in the versions this targets, and `JSON_VALUE` / `OPENJSON` / `ISJSON` all read `nvarchar`
anyway. A decimal needing more than T-SQL's 38 digits of precision (or a scale above 37) is
**refused** rather than rounded.

Rows come back in the same JSON shape as every other backend: `numeric` as an exact string, blobs as
arrays of byte values, UUIDs and dates as strings. Which form a timestamp takes is decided by the
column: a `datetimeoffset` column renders as RFC 3339 and a `chrono::DateTime<Utc>` field
round-trips, while `datetime2` / `datetime` render in chrono's zoneless textual form and need a
`chrono::NaiveDateTime` field — the zone is not guessed at, because guessing wrong would silently
shift every value in the column. Declare timestamp columns `datetimeoffset`.

One difference from the sqlx backends: a column with **no name** is an error rather than an empty
key, so give `COUNT(*)` an `AS` alias.

**Errors.** A refused login, a disabled account, a firewall rejection or a missing `GRANT` becomes
`DbError::Unauthorized`; a resource-governance limit becomes `DbError::Throttled`, carrying Azure's
own retry delay when the message states one; a deadlock victim (`1205`) becomes `DbError::Conflict`.
Error `40613` — the database is resuming from a serverless pause — is deliberately *not* throttling:
it is transient, but telling a caller to back off would be the wrong diagnosis.

**Runtime.** tiberius is Tokio-based, like the rest of this crate, so the backend must be built and
used inside a Tokio runtime (`#[skyzen::main]` and `skyzen-lambda` both provide one). TLS is
`rustls`. The pool validates a connection before handing it out, which costs one extra round trip
per checkout and is what keeps Azure's idle-connection reaping from surfacing as a failed query.
There is no wait timeout: a request that arrives while every connection is checked out waits for one
rather than failing fast, so bound request time with Skyzen's `Timeout` middleware and size
`with_max_pool_size` against the concurrency you expect.

A `DbValue::Null` has to declare *some* type — TDS has no untyped null — and goes out as a null
`nvarchar`, which is what ADO.NET does with an unspecified `DBNull`. SQL Server converts a null of
one type to a null of another wherever the column needs it.

**Not covered offline.** The unit tests here exercise everything reachable without a server —
connection-string policy, the `DbValue` → TDS mapping, row conversion, the error taxonomy. What
needs a live database: the TLS handshake against a real Azure SQL endpoint, the transaction
semantics through tiberius's `sp_executesql` path, Azure's exact throttling message text, a null
`nvarchar` parameter against a non-string column, and the type round-trips end to end.

## Wiring

Inject a backend the way any other Skyzen service is injected, and the handlers never mention Azure:

```rust
Route::new(("/files".at(list_files),))
    .with(Kv::new(cosmos))
    .with(Storage::new(blob))
    .with(Queue::new(jobs))
    .with(Db::new(sql))
    .build()
```

## License

MIT or Apache-2.0, at your option.
