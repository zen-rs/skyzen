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
| `servicebus` | Yes | Service Bus `MessageQueue` via `azure_messaging_servicebus` |
| `storage-queue` | Yes | Azure Storage queue `MessageQueue` via `azure_storage_queue` |

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

## Wiring

Inject a backend the way any other Skyzen service is injected, and the handlers never mention Azure:

```rust
Route::new(("/files".at(list_files),))
    .with(Kv::new(cosmos))
    .with(Storage::new(blob))
    .with(Queue::new(jobs))
    .build()
```

## License

MIT or Apache-2.0, at your option.
