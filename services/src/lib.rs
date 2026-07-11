#![cfg_attr(target_arch = "wasm32", allow(clippy::future_not_send))]

//! Portable service abstractions for the Skyzen framework.
//!
//! This crate defines platform-agnostic traits and extractors for common
//! backend services: key-value stores, object storage, message queues, and databases.
//!
//! Each service follows a two-layer design:
//! - A **public trait** (e.g. [`KeyValueStore`]) that is ergonomic for implementors
//! - A **wrapper struct** (e.g. [`Kv`]) that provides type-erased dynamic dispatch
//!   and implements [`skyzen_core::Extractor`] for use in handlers
//!
//! Portable SQL support is provided through [`Db`].
//!
//! # Example
//!
//! ```ignore
//! use skyzen_services::{Db, Kv, Storage};
//!
//! async fn handler(kv: Kv, db: Db) -> Result<Json<Value>> {
//!     kv.put_json("cache:key", &json!({"hello": "world"})).await?;
//!     let result: Vec<Value> = db.query("SELECT * FROM users").fetch_all().await?;
//!     Ok(Json(result))
//! }
//! ```

#[cfg(all(target_arch = "wasm32", feature = "sqlite"))]
compile_error!(
    "Feature `sqlite` is not supported on wasm32 targets. \
Use cloud vendor database services instead (for Cloudflare, use skyzen-cloudflare::CfD1 or CfDurableSqlite / skyzen_services::durable::DurableDb)."
);

#[macro_use]
mod macros;

pub mod durable;
pub mod events;
pub mod kv;
mod maybe_send;
pub mod queue;
pub mod sql;
pub mod storage;

pub use durable::{DurableDb, DurableDbBackend, DurableDbError};
pub use events::ScheduledTick;
pub use kv::{KeyValueStore, Kv, KvError};
pub use queue::{
    MessageQueue, Queue, QueueBatch, QueueBatchDisposition, QueueError, QueueMessage,
    QueueMessageDisposition, QueueRetry,
};
pub use sql::{Db, DbBackend, DbError, DbExecResult, DbTransaction, DbTransactionBackend, DbValue};
pub use storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, Storage, StorageError, StorageObject,
};
