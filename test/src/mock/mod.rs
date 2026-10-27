//! Mock service implementations for testing.
//!
//! All mocks are in-memory and isolated per instance.
//! `InMemoryDb` uses `SQLite` memory mode and requires the crate runtime feature.

#[cfg(feature = "runtime-tokio")]
pub mod db;
pub mod durable;
pub mod kv;
pub mod queue;
pub mod storage;

#[cfg(feature = "runtime-tokio")]
pub use db::InMemoryDb;
#[cfg(feature = "runtime-tokio")]
pub use durable::InMemoryDurableDb;
pub use durable::{InMemoryAlarm, InMemoryDurableKv};
pub use kv::InMemoryKv;
pub use queue::InMemoryQueue;
pub use storage::InMemoryStorage;
