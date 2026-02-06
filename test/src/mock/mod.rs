//! Mock service implementations for testing.
//!
//! All mocks are in-memory, isolated per instance, and require no external dependencies.

pub mod kv;
pub mod queue;
pub mod storage;

pub use kv::InMemoryKv;
pub use queue::InMemoryQueue;
pub use storage::InMemoryStorage;
