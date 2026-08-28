//! Mock implementations for Durable Object services.

pub mod alarm;
pub mod kv;
#[cfg(any(feature = "runtime-tokio-native-tls", feature = "runtime-tokio-rustls"))]
pub mod sql;

pub use alarm::InMemoryAlarm;
pub use kv::InMemoryDurableKv;
#[cfg(any(feature = "runtime-tokio-native-tls", feature = "runtime-tokio-rustls"))]
pub use sql::InMemoryDurableDb;
