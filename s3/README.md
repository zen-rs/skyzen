# skyzen-s3

[![Crates.io](https://img.shields.io/crates/v/skyzen-s3.svg)](https://crates.io/crates/skyzen-s3)
[![Docs.rs](https://docs.rs/skyzen-s3/badge.svg)](https://docs.rs/skyzen-s3)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

S3-compatible `ObjectStorage` implementation for the Skyzen framework.

## Overview

`skyzen-s3` provides a robust, production-ready backend for the Skyzen `ObjectStorage` abstraction. It is built on top of the official `aws-sdk-s3` crate and is compatible with:

- **AWS S3**: The industry-standard object storage.
- **MinIO**: High-performance, S3-compatible self-hosted storage.
- **Cloudflare R2**: Edge-ready storage via the S3 API.
- **LocalStack**: For local cloud development and testing.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
skyzen-s3 = "0.1"
skyzen-services = "0.1"
skyzen = "0.1"
```

## Quick Start

The following example demonstrates how to initialize `S3Storage` and inject it into a Skyzen application as middleware.

```rust
use skyzen::prelude::*;
use skyzen_s3::S3Storage;
use skyzen_services::Storage;

#[skyzen::main]
async fn main() -> Result<()> {
    // 1. Initialize S3 backend from environment variables
    // (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, etc.)
    let s3 = S3Storage::from_env("my-app-assets").await;

    // 2. Wrap in the type-erased Storage service
    let storage = Storage::new(s3);

    // 3. Inject as middleware to make it available to handlers
    let router = Router::new()
        .at("/upload").post(upload_handler)
        .with(storage);

    Server::new(router).run().await
}

// 4. Use the Storage extractor in your handler
async fn upload_handler(storage: Storage) -> Result<impl Responder> {
    storage.put("hello.txt", b"Hello from Skyzen!".to_vec()).await?;
    Ok(StatusCode::OK)
}
```

## Core Concepts

### `S3Storage` Struct

`S3Storage` is the primary type in this crate. It wraps the AWS SDK client and manages bucket operations.

- **`S3Storage::from_env(bucket)`**: Best for production. Automatically loads configuration from standard AWS environment variables or IAM roles.
- **`S3Storage::with_endpoint(bucket, url)`**: Best for custom S3 providers like MinIO or local development.
- **`S3Storage::new(client, bucket)`**: Provides full control by accepting a pre-configured `aws_sdk_s3::Client`.

## Examples

### Using MinIO for Local Development

When developing locally with MinIO, you can specify the endpoint and bypass certificate verification if needed:

```rust
use skyzen_s3::S3Storage;

async fn setup_minio() -> S3Storage {
    S3Storage::with_endpoint(
        "local-bucket",
        "http://localhost:9000"
    ).await
}
```

### Advanced Manual Setup

If you need custom retry logic or specific region settings, you can build the client manually:

```rust
use aws_sdk_s3::Client;
use skyzen_s3::S3Storage;

async fn manual_setup() -> S3Storage {
    let config = aws_config::load_from_env().await;
    let client = Client::new(&config);
    S3Storage::new(client, "my-custom-bucket")
}
```

## API Overview

| Method | Description |
|--------|-------------|
| `get(key)` | Retrieves an object and its metadata. |
| `put(key, body)` | Uploads a byte buffer to the specified key. |
| `put_with(key, body, options)` | Uploads with content type, custom metadata, cache control, content encoding/disposition, a storage class, and an MD5 the service verifies. |
| `delete(key)` | Removes an object from the bucket. |
| `list(options)` | Lists objects with optional prefix, limit, and pagination. |
| `head(key)` | Retrieves object metadata without the body. |
| `get_stream(key)` | Streams the body off S3 in chunks, without buffering the object. |
| `put_stream(key, stream, length, options)` | Uploads from a stream; bodies past 16 MiB become a real multipart upload with 8 MiB parts, aborted on any failure. |
| `get_range(key, range)` | Serves an HTTP `Range` — the body is the slice, the metadata still reports the whole object's size. |
| `presign_get(key, expires_in)` | Mints a URL a browser can download from directly. |
| `presign_put(key, expires_in, options)` | Mints a URL a browser can upload to directly, keeping large uploads off the application server. |

`list` reports only what `ListObjectsV2` returns — key, size, last-modified and ETag — so
`content_type` is always `None` and `metadata` always empty in a listing. Filling them in would
cost one `HeadObject` per key; call `head(key)` for the object you actually need.

## Related Crates

- `skyzen-services`: Defines the `ObjectStorage` trait and `Storage` extractor.
- `skyzen-test`: Provides an `InMemoryStorage` implementation for unit testing your handlers.
- `skyzen-cloudflare`: Native R2 bindings for WASM-based edge deployments.
