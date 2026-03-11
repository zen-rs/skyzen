//! Mock service implementations for testing.
//!
//! All mocks are in-memory and isolated per instance.
//! `InMemoryDb` uses `SQLite` memory mode and requires the crate runtime feature.

#[cfg(any(feature = "runtime-tokio-native-tls", feature = "runtime-tokio-rustls"))]
pub mod db;
pub mod durable;
pub mod kv;
pub mod queue;
pub mod storage;

#[cfg(any(feature = "runtime-tokio-native-tls", feature = "runtime-tokio-rustls"))]
pub use db::InMemoryDb;
pub use durable::{InMemoryAlarm, InMemoryDurableKv, InMemoryDurableSql};
pub use kv::InMemoryKv;
pub use queue::InMemoryQueue;
pub use storage::InMemoryStorage;
