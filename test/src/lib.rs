//! Testing utilities and mock implementations for the Skyzen framework.
//!
//! This crate provides:
//! - **Mock implementations** for all service traits (`InMemoryKv`, `InMemoryStorage`, `InMemoryQueue`)
//! - **`InMemoryDb`** for `SQLite` in-memory database tests (requires a runtime feature)
//! - **`TestContext`** for creating HTTP test clients
//! - **`TestClient`** for sending requests to endpoints without network I/O
//! - **Response assertions** for validating status codes, headers, and body content
//! - **Snapshot testing** via [`insta`]
//! - **Fixture loading** helpers for JSON test data
//!
//! # Example
//!
//! ```ignore
//! use skyzen_test::{TestContext, mock::InMemoryKv};
//! use skyzen_services::Kv;
//!
//! #[skyzen::test]
//! async fn test_handler(kv: Kv, ctx: TestContext) {
//!     kv.put("key", b"value").await.unwrap();
//!     let client = ctx.client(my_app());
//!     let response = client.get("/key").send().await;
//!     response.assert_status(200);
//! }
//! ```

pub mod assertions;
pub mod client;
pub mod context;
pub mod events;
pub mod fixtures;
pub mod mock;
pub mod snapshot;

pub use assertions::TestResponse;
pub use client::TestClient;
pub use context::TestContext;
pub use events::{queue_batch, queue_message, scheduled_tick};
pub use snapshot::SnapshotExt;
