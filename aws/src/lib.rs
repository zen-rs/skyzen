//! AWS service implementations for the Skyzen framework.
//!
//! This crate provides AWS implementations of the service traits
//! defined in `skyzen-services`:
//!
//! - [`DynamoKv`] — Amazon `DynamoDB` (implements [`KeyValueStore`])
//! - [`S3Storage`] — Amazon S3 (re-exported from `skyzen-s3`, implements [`ObjectStorage`])
//! - [`SqsQueue`] — Amazon SQS (implements [`MessageQueue`])
//!
//! # Example
//!
//! ```ignore
//! use skyzen_aws::{DynamoKv, SqsQueue};
//! use skyzen_services::{Kv, Queue};
//!
//! let kv = Kv::new(DynamoKv::from_env("my-table").await);
//! let queue = Queue::new(
//!     SqsQueue::from_env("https://sqs.us-east-1.amazonaws.com/123/my-queue").await?,
//! );
//! ```
//!
//! [`KeyValueStore`]: skyzen_services::kv::KeyValueStore
//! [`ObjectStorage`]: skyzen_services::storage::ObjectStorage
//! [`MessageQueue`]: skyzen_services::queue::MessageQueue

#[cfg(any(feature = "dynamodb", feature = "sqs"))]
mod errors;

#[cfg(feature = "dynamodb")]
pub mod dynamodb;
#[cfg(feature = "sqs")]
pub mod sqs;

#[cfg(feature = "dynamodb")]
pub use dynamodb::DynamoKv;
#[cfg(feature = "s3")]
pub use skyzen_s3::S3Storage;
#[cfg(feature = "sqs")]
pub use sqs::{SqsDeduplication, SqsQueue};
