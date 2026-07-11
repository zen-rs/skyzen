//! Azure service implementations for the Skyzen framework.
//!
//! This crate provides Azure implementations of the service traits
//! defined in `skyzen-services`:
//!
//! - [`CosmosKv`] — Azure Cosmos DB (implements [`KeyValueStore`])
//! - [`AzureBlob`] — Azure Blob Storage (implements [`ObjectStorage`])
//! - [`ServiceBusQueue`] — Azure Service Bus (implements [`MessageQueue`])
//!
//! # Example
//!
//! ```ignore
//! use skyzen_azure::{CosmosKv, AzureBlob, ServiceBusQueue};
//! use skyzen_services::{Kv, Storage, Queue};
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

#[cfg(feature = "blob")]
pub use blob::AzureBlob;
#[cfg(feature = "cosmos")]
pub use cosmos::CosmosKv;
#[cfg(feature = "servicebus")]
pub use service_bus::ServiceBusQueue;
