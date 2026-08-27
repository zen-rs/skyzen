//! Azure service implementations for the Skyzen framework.
//!
//! This crate provides Azure implementations of the service traits
//! defined in `skyzen-services`:
//!
//! - [`CosmosKv`] — Azure Cosmos DB (implements [`KeyValueStore`])
//! - [`AzureBlob`] — Azure Blob Storage (implements [`ObjectStorage`])
//! - [`ServiceBusQueue`] — Azure Service Bus (implements [`MessageQueue`])
//! - [`AzureStorageQueue`] — Azure Storage queues (implements [`MessageQueue`])
//!
//! # Choosing a queue
//!
//! Azure has two queues and they are not interchangeable. Service Bus is the richer broker —
//! sessions, topics, scheduled delivery, dead-lettering — and it is what an application that needs
//! ordering or transactions wants. Azure Storage queues are the simpler and cheaper one: a flat
//! queue of text messages up to 64 KB with visibility timeouts, backed by a storage account you
//! probably already have. Both implement [`MessageQueue`] in full, so the choice is a
//! configuration one rather than a code one.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_azure::{AzureBlob, CosmosKv, ServiceBusQueue};
//! use skyzen_services::{Kv, Queue, Storage};
//!
//! let kv = Kv::new(CosmosKv::from_env("app", "kv").await?);
//! let storage = Storage::new(AzureBlob::from_env("uploads")?);
//! let queue = Queue::new(ServiceBusQueue::from_env("jobs")?);
//! ```
//!
//! [`KeyValueStore`]: skyzen_services::kv::KeyValueStore
//! [`ObjectStorage`]: skyzen_services::storage::ObjectStorage
//! [`MessageQueue`]: skyzen_services::queue::MessageQueue

#[cfg(feature = "blob")]
pub mod blob;
#[cfg(feature = "cosmos")]
pub mod cosmos;
#[cfg(feature = "servicebus")]
pub mod service_bus;
#[cfg(any(
    feature = "blob",
    feature = "cosmos",
    feature = "servicebus",
    feature = "storage-queue"
))]
mod status;
#[cfg(feature = "storage-queue")]
pub mod storage_queue;

#[cfg(feature = "blob")]
pub use blob::{AzureBlob, AzureBlobAuth, AzureBlobConfig};
#[cfg(feature = "cosmos")]
pub use cosmos::{CosmosKv, CosmosKvBuilder, PartitionStrategy};
#[cfg(feature = "servicebus")]
pub use service_bus::ServiceBusQueue;
#[cfg(feature = "storage-queue")]
pub use storage_queue::AzureStorageQueue;
