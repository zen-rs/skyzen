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

## Installation

```toml
[dependencies]
skyzen-azure = "0.1"
```

### Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `cosmos` | Yes | Cosmos DB `KeyValueStore` via `azure_data_cosmos` |
| `blob` | Yes | Blob Storage `ObjectStorage` via `azure_storage_blob` |
| `servicebus` | Yes | Service Bus `MessageQueue` via `azure_messaging_servicebus` |

Disable unused features to reduce compile times:

```toml
skyzen-azure = { version = "0.1", default-features = false, features = ["cosmos", "blob"] }
```

## Usage

### CosmosKv

Stores key-value pairs as JSON documents with `id`, base64-encoded `value`, and `partition_key` fields:

```rust
use skyzen_azure::CosmosKv;
use skyzen_services::Kv;

let cosmos = CosmosKv::new(client, "my-database", "my-container");
let kv = Kv::new(cosmos);

kv.put_json("user:1", &user).await?;
let user: Option<User> = kv.get_json("user:1").await?;
```

### AzureBlob

```rust
use skyzen_azure::AzureBlob;
use skyzen_services::Storage;

let blob = AzureBlob::new(client, "my-container");
let storage = Storage::new(blob);

storage.put("images/photo.png", image_bytes).await?;
let obj = storage.get("images/photo.png").await?;
```

### ServiceBusQueue

> Note: Batch sends are processed individually (Service Bus SDK limitation).

```rust
use skyzen_azure::ServiceBusQueue;
use skyzen_services::Queue;

let sb = ServiceBusQueue::new(client, "my-queue");
let queue = Queue::new(sb);

queue.send_json(&event).await?;
```

## Skyzen.toml

```toml
[azure]
project = "."
app_name = "my-function-app"
port = 7071
```

## License

MIT or Apache-2.0, at your option.
