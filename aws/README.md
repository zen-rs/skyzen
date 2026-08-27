# skyzen-aws

[![Crates.io](https://img.shields.io/crates/v/skyzen-aws.svg)](https://crates.io/crates/skyzen-aws)
[![Documentation](https://docs.rs/skyzen-aws/badge.svg)](https://docs.rs/skyzen-aws)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

AWS platform implementations for the [Skyzen](https://github.com/zen-rs/skyzen) HTTP framework.

## Overview

`skyzen-aws` provides high-performance, async-ready implementations of Skyzen service traits using the official AWS SDK for Rust. It enables your Skyzen applications to interact with AWS services like DynamoDB, SQS, and S3 using platform-agnostic abstractions.

## Services Provided

- **`DynamoKv`**: A `KeyValueStore` implementation using Amazon DynamoDB.
  - Uses a configurable partition key (defaults to `"pk"`).
  - Stores values as binary (`B`) attributes in a `"value"` column.
  - Supports prefix-based listing via Scan operations.
  - Atomic primitives (`put_if_absent`, `compare_and_swap`, `increment`, `expire`) run on DynamoDB
    condition expressions, and treat an item whose TTL has passed as absent even before DynamoDB's
    lazy sweeper removes it.
  - `with_consistent_reads(true)` opts `get`/`exists` into `ConsistentRead`.
- **`SqsQueue`**: A `MessageQueue` implementation using Amazon SQS.
  - Automatically handles Base64 encoding/decoding for binary payloads (since SQS is text-based),
    tagged with a `skyzen-content-encoding` message attribute that `receive` reverses.
  - Full pull consumption: `receive` (long polling, visibility timeout), `ack` (`DeleteMessage`)
    and `nack` (`ChangeMessageVisibility`).
  - Supports batch sending (automatically chunked into batches of 10), reporting rejected entries
    as `QueueError::PartialBatch` with each failure's index in the caller's slice.
  - FIFO queues via `SqsQueue::fifo`, which sends a `MessageGroupId` with every message; a `.fifo`
    URL without a group id (or a standard URL with one) is refused at construction.
- **`S3Storage`**: Amazon S3 implementation of `ObjectStorage` (re-exported from `skyzen-s3`).
- **`RdsDataDb`**: A `DbBackend` implementation running SQL on Aurora through the RDS Data API — an
  HTTP endpoint, so it needs no connection and no pool, which is what makes it usable from a Lambda
  or any runtime that cannot hold a socket open.
  - Aurora PostgreSQL and Aurora MySQL, chosen with `RdsEngine`, which decides the SQL dialect.
  - `?` placeholders are rewritten to the Data API's named parameters with `sqlparser`'s tokenizer,
    so a `?` inside a string literal or a comment is never mistaken for a bind.
  - Rich parameters (`Timestamp`, `Uuid`, `Decimal`, `Json`) bind through the service's type hints
    instead of being stringified by the caller.
  - **Real interactive transactions** — `BeginTransaction` / `CommitTransaction` /
    `RollbackTransaction` — which no other serverless backend in Skyzen offers, plus
    `execute_batch` as one all-or-nothing transaction.
  - See the module documentation for the service's limits (a 1 MiB response cap, a 45-second
    statement timeout, writer instances only) and for how its rows differ from the sqlx backends'.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
skyzen-aws = "0.1"
# Individual features can be toggled
# skyzen-aws = { version = "0.1", default-features = false, features = ["dynamodb"] }
```

### Feature Flags

- `dynamodb`: Enables DynamoDB support (default)
- `sqs`: Enables SQS support (default)
- `s3`: Enables S3 support (default)
- `rds-data`: Enables Aurora RDS Data API support (default)

## Configuration

All services provide a `from_env()` constructor that uses `aws_config::load_defaults` to resolve credentials, region, and endpoints from environment variables, IAM roles, or AWS profile files.

### Skyzen.toml Example

`[aws]` in `Skyzen.toml` configures the **Lambda deployment**, not these clients — the SDK resolves
its own credentials and region the way every AWS tool does. It is what `skyzen deploy --provider
aws` tells `cargo lambda`:

```toml
[aws]
function_name = "skyzen-api"   # optional; defaults to the binary's own name
memory_mb = 512                # Lambda scales CPU with memory
timeout = "30s"                # humantime; sent to Lambda as whole seconds
architecture = "arm64"         # "arm64" (default) or "x86_64"
url = true                     # create and keep a Function URL; the default

[aws.env]                      # plaintext environment variables set on the function
RUST_LOG = "info"
```

See the [Skyzen.toml reference](../docs/skyzen-toml-reference.md#aws-section) for the full table.

## Quick Start

### DynamoDB as Key-Value Store

```rust
use skyzen_aws::DynamoKv;
use skyzen_services::Kv;

#[tokio::main]
async fn main() {
    // Initialize from environment (default partition key: "pk")
    let backend = DynamoKv::from_env("my-table-name").await;
    let kv = Kv::new(backend);

    // Use the type-erased wrapper
    kv.put("user:123", b"data").await.unwrap();
    let data = kv.get("user:123").await.unwrap();
}
```

### SQS Message Queue

```rust
use skyzen_aws::SqsQueue;
use skyzen_services::{Queue, ReceiveOptions};

#[tokio::main]
async fn main() {
    let queue_url = "https://sqs.us-east-1.amazonaws.com/123456789012/my-queue";
    let backend = SqsQueue::from_env(queue_url).await.unwrap();
    let queue = Queue::new(backend);

    // Send binary payload (auto-encoded to Base64)
    queue.send(b"important-task").await.unwrap();

    // Pull it back, then settle the lease
    for message in queue.receive(ReceiveOptions::new()).await.unwrap() {
        queue.ack(&message.receipt).await.unwrap();
    }
}
```

A FIFO queue needs a message group id on every send, so it is built with a different constructor —
`SqsQueue::from_env` refuses a `.fifo` URL rather than failing on the first message:

```rust
use skyzen_aws::{SqsDeduplication, SqsQueue};

#[tokio::main]
async fn main() {
    let queue_url = "https://sqs.us-east-1.amazonaws.com/123456789012/orders.fifo";
    let backend = SqsQueue::fifo_from_env(queue_url, "customer-42")
        .await
        .unwrap()
        // Only needed when the queue does not have ContentBasedDeduplication enabled.
        .with_deduplication(SqsDeduplication::ContentHash)
        .unwrap();
}
```

### Aurora SQL through the RDS Data API

`RdsDataDb::from_env` reads `RDS_RESOURCE_ARN`, `RDS_SECRET_ARN`, `RDS_DATABASE` and `RDS_ENGINE`
(`aurora-postgresql` or `aurora-mysql`), and refuses to start on an engine name it does not know:

```rust
use serde::Deserialize;
use skyzen_aws::RdsDataDb;
use skyzen_services::Db;

#[derive(Deserialize)]
struct User {
    id: i64,
    email: String,
}

#[tokio::main]
async fn main() {
    let db = Db::new(RdsDataDb::from_env().await.unwrap());

    // `?` on every dialect: Skyzen rewrites it for the engine, and this backend rewrites it again
    // into the Data API's named parameters.
    let user: User = db
        .query("SELECT id, email FROM users WHERE id = ?")
        .bind(7_i64)
        .fetch_one()
        .await
        .unwrap();

    // A real transaction, not a batch pretending to be one.
    let mut transaction = db.begin().await.unwrap();
    transaction
        .query("UPDATE accounts SET balance = balance - ? WHERE id = ?")
        .bind(100_i64)
        .bind(user.id)
        .execute()
        .await
        .unwrap();
    transaction.commit().await.unwrap();
}
```

## Related Crates

- `skyzen-core`: Foundational traits
- `skyzen-services`: Service abstractions and type-erased wrappers
- `skyzen-s3`: Dedicated S3 storage implementation
