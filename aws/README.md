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

## Configuration

All services provide a `from_env()` constructor that uses `aws_config::load_defaults` to resolve credentials, region, and endpoints from environment variables, IAM roles, or AWS profile files.

### Skyzen.toml Example

When using the Skyzen CLI or runtime helpers, you can configure AWS settings in your `Skyzen.toml`:

```toml
[aws]
template = "cloudformation.yaml"
stack_name = "my-app-prod"
region = "us-east-1"
profile = "default"
local_port = 4566 # For LocalStack development
```

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

## Related Crates

- `skyzen-core`: Foundational traits
- `skyzen-services`: Service abstractions and type-erased wrappers
- `skyzen-s3`: Dedicated S3 storage implementation
