//! S3-compatible [`ObjectStorage`] implementation for the Skyzen framework.
//!
//! This crate provides an [`S3Storage`] type that implements the [`ObjectStorage`] trait
//! from `skyzen-services`, enabling any S3-compatible service (AWS S3, `MinIO`, etc.)
//! as an object storage backend.
//!
//! # Example
//!
//! ```ignore
//! use skyzen_s3::S3Storage;
//! use skyzen_services::Storage;
//!
//! let s3 = S3Storage::from_env("my-bucket").await;
//! let storage = Storage::new(s3);
//! storage.put("key.txt", b"hello".to_vec()).await?;
//! ```

use std::collections::HashMap;

use aws_sdk_s3::config::Builder as S3ConfigBuilder;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, StorageError, StorageObject,
};

/// An S3-compatible object storage backend.
///
/// Supports any S3-compatible service including AWS S3, `MinIO`, Cloudflare R2,
/// and other compatible providers via custom endpoint configuration.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct S3Storage {
    client: Client,
    bucket: String,
}

impl S3Storage {
    /// Create a new `S3Storage` from an existing [`Client`] and bucket name.
    pub fn new(client: Client, bucket: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
        }
    }

    /// Create a new `S3Storage` from environment configuration.
    ///
    /// Uses the default AWS SDK configuration loader, which reads
    /// from environment variables, config files, and instance metadata.
    pub async fn from_env(bucket: impl Into<String>) -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&config);
        Self::new(client, bucket)
    }

    /// Create a new `S3Storage` with a custom endpoint URL.
    ///
    /// Use this for S3-compatible services like `MinIO` or local development.
    pub async fn with_endpoint(bucket: impl Into<String>, endpoint_url: impl Into<String>) -> Self {
        let shared_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let s3_config = S3ConfigBuilder::from(&shared_config)
            .endpoint_url(endpoint_url)
            .force_path_style(true)
            .build();
        let client = Client::from_conf(s3_config);
        Self::new(client, bucket)
    }
}

/// Convert an S3 `DateTime` to seconds since Unix epoch.
fn timestamp_secs(dt: &aws_smithy_types::DateTime) -> Option<u64> {
    u64::try_from(dt.secs()).ok()
}

/// Convert an S3 i64 size to u64, clamping negatives to 0.
fn size_u64(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

impl ObjectStorage for S3Storage {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match result {
            Ok(output) => {
                let content_type = output.content_type().map(ToOwned::to_owned);
                let last_modified = output.last_modified().and_then(timestamp_secs);
                let user_metadata = output.metadata().cloned().unwrap_or_default();

                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| StorageError::Io(e.to_string()))?
                    .to_vec();

                let metadata = ObjectMetadata {
                    key: key.to_owned(),
                    size: body.len() as u64,
                    content_type,
                    last_modified,
                    metadata: user_metadata,
                };

                Ok(Some(StorageObject { body, metadata }))
            }
            Err(err) => {
                if is_not_found(&err) {
                    Ok(None)
                } else {
                    Err(StorageError::Backend(err.to_string()))
                }
            }
        }
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        let mut request = self.client.list_objects_v2().bucket(&self.bucket);

        if let Some(ref prefix) = options.prefix {
            request = request.prefix(prefix);
        }

        if let Some(limit) = options.limit {
            request = request.max_keys(i32::try_from(limit).unwrap_or(i32::MAX));
        }

        if let Some(ref cursor) = options.cursor {
            request = request.continuation_token(cursor);
        }

        let output = request
            .send()
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;

        let objects = output
            .contents()
            .iter()
            .map(|obj| ObjectMetadata {
                key: obj.key().unwrap_or_default().to_owned(),
                size: size_u64(obj.size().unwrap_or_default()),
                content_type: None,
                last_modified: obj.last_modified().and_then(timestamp_secs),
                metadata: HashMap::new(),
            })
            .collect();

        let cursor = output.next_continuation_token().map(ToOwned::to_owned);

        Ok(ListResult { objects, cursor })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;

        match result {
            Ok(output) => {
                let content_type = output.content_type().map(ToOwned::to_owned);
                let last_modified = output.last_modified().and_then(timestamp_secs);
                let size = size_u64(output.content_length().unwrap_or_default());

                Ok(Some(ObjectMetadata {
                    key: key.to_owned(),
                    size,
                    content_type,
                    last_modified,
                    metadata: output.metadata().cloned().unwrap_or_default(),
                }))
            }
            Err(err) => {
                if is_not_found(&err) {
                    Ok(None)
                } else {
                    Err(StorageError::Backend(err.to_string()))
                }
            }
        }
    }
}

/// Check if an S3 error is a "not found" (`NoSuchKey` / 404).
fn is_not_found<E: std::fmt::Display>(err: &aws_sdk_s3::error::SdkError<E>) -> bool {
    matches!(
        err,
        aws_sdk_s3::error::SdkError::ServiceError(service_err)
            if service_err.raw().status().as_u16() == 404
    )
}
