//! AWS service implementations for the Skyzen framework.
//!
//! This crate provides AWS implementations of the service traits
//! defined in `skyzen-services`:
//!
//! - [`DynamoKv`] — Amazon `DynamoDB` (implements [`KeyValueStore`])
//! - [`SqsQueue`] — Amazon SQS (implements [`MessageQueue`])
//! - [`RdsDataDb`] — Aurora through the RDS Data API (implements [`DbBackend`])
//!
//! Amazon S3 is `skyzen-s3`'s `S3Storage`, built on this crate's [`sdk`] module.
//!
//! # Runtime
//!
//! The SDK clients do their I/O through [`sdk`]: requests on zenwave's runtime-neutral native
//! backend, timers on async-io. Every backend here works under any executor — compio's
//! thread-per-core runtime included — and needs no tokio context.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_aws::{DynamoKv, RdsDataDb, SqsQueue};
//! use skyzen_services::{Db, Kv, Queue};
//!
//! let kv = Kv::new(DynamoKv::from_env("my-table").await);
//! let queue = Queue::new(
//!     SqsQueue::from_env("https://sqs.us-east-1.amazonaws.com/123/my-queue").await?,
//! );
//! let db = Db::new(RdsDataDb::from_env().await?);
//! ```
//!
//! [`KeyValueStore`]: skyzen_services::kv::KeyValueStore
//! [`ObjectStorage`]: skyzen_services::storage::ObjectStorage
//! [`MessageQueue`]: skyzen_services::queue::MessageQueue
//! [`DbBackend`]: skyzen_services::sql::DbBackend

#[cfg(any(feature = "dynamodb", feature = "rds-data", feature = "sqs"))]
mod errors;

#[cfg(feature = "dynamodb")]
pub mod dynamodb;
#[cfg(feature = "rds-data")]
pub mod rds_data;
pub mod sdk;
#[cfg(feature = "sqs")]
pub mod sqs;

#[cfg(feature = "dynamodb")]
pub use dynamodb::DynamoKv;
#[cfg(feature = "rds-data")]
pub use rds_data::{RdsDataDb, RdsDataTransaction, RdsEngine};
#[cfg(feature = "sqs")]
pub use sqs::{SqsDeduplication, SqsQueue};
