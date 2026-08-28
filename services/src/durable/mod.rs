//! Durable Object service abstractions.
//!
//! Provides platform-agnostic traits for Durable Object services:
//! key-value storage, SQL storage, and alarm scheduling.
//!
//! Each service follows the same two-layer design as the top-level services:
//! - A **public trait** (e.g. [`DurableKvStore`]) that is ergonomic for implementors
//! - A **wrapper struct** (e.g. [`DurableKv`]) that provides type-erased dynamic dispatch
//!   and implements [`skyzen_core::Extractor`] for use in handlers

pub mod alarm;
pub mod kv;
pub mod sql;
#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
mod sqlite;

pub use crate::sql::{DbExecResult, DbValue};
pub use alarm::{Alarm, AlarmError, AlarmScheduler};
pub use kv::{DurableKv, DurableKvError, DurableKvStore, DurableListOptions};
pub use sql::{DurableDb, DurableDbBackend, DurableDbError, DurableDbQuery};
#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
pub use sqlite::SqliteDurableDb;
