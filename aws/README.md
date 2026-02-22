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
- **`SqsQueue`**: A `MessageQueue` implementation using Amazon SQS.
  - Automatically handles Base64 encoding/decoding for binary payloads (since SQS is text-based).
  - Supports batch sending (automatically chunked into batches of 10).
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
use skyzen_services::Queue;

#[tokio::main]
async fn main() {
    let queue_url = "https://sqs.us-east-1.amazonaws.com/123456789012/my-queue";
    let backend = SqsQueue::from_env(queue_url).await;
    let queue = Queue::new(backend);

    // Send binary payload (auto-encoded to Base64)
    queue.send(b"important-task").await.unwrap();
}
```

## Related Crates

- `skyzen-core`: Foundational traits
- `skyzen-services`: Service abstractions and type-erased wrappers
- `skyzen-s3`: Dedicated S3 storage implementation
