//! Mock service implementations for testing.
//!
//! All mocks are in-memory and isolated per instance.
//! `InMemoryDb` uses `SQLite` memory mode and requires the crate's `sqlite` feature.

#[cfg(feature = "sqlite")]
pub mod db;
pub mod durable;
pub mod kv;
pub mod queue;
pub mod storage;

#[cfg(feature = "sqlite")]
pub use db::InMemoryDb;
#[cfg(feature = "sqlite")]
pub use durable::InMemoryDurableDb;
pub use durable::{InMemoryAlarm, InMemoryDurableKv};
pub use kv::InMemoryKv;
pub use queue::InMemoryQueue;
pub use storage::InMemoryStorage;
