// On WASM, BoxFuture is LocalBoxFuture (not Send), so wrapper methods are correctly !Send.
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
//! Database support is provided through `SeaORM`, re-exported for convenience.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_services::{Kv, Db, Storage, Queue};
//! use skyzen_services::sea_orm::*;
//!
//! async fn handler(kv: Kv, db: Db) -> Result<Json<Value>> {
//!     kv.put_json("cache:key", &json!({"hello": "world"})).await?;
//!     let result = entity::Entity::find().all(&*db).await?;
//!     Ok(Json(result))
//! }
//! ```

#[cfg(all(target_arch = "wasm32", feature = "sqlite"))]
compile_error!(
    "Feature `sqlite` is not supported on wasm32 targets. \
Use cloud vendor database services instead (for Cloudflare, use skyzen-cloudflare::CfD1 or CfDurableSqlite)."
);

#[cfg(not(target_arch = "wasm32"))]
pub mod database;
pub mod kv;
mod maybe_send;
pub mod queue;
pub mod storage;

#[cfg(not(target_arch = "wasm32"))]
pub use database::Db;
pub use kv::{KeyValueStore, Kv, KvError};
pub use queue::{MessageQueue, Queue, QueueError};
pub use storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, Storage, StorageError, StorageObject,
};

/// Re-export `SeaORM` for user convenience.
///
/// Users can write `use skyzen_services::sea_orm::*;` to access the full `SeaORM` API.
#[cfg(not(target_arch = "wasm32"))]
pub use sea_orm;
