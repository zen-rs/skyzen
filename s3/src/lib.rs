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
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use aws_smithy_types::error::display::DisplayErrorContext;
use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, PutOptions, StorageError, StorageObject,
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

/// Map an AWS SDK error to a [`StorageError::Backend`] with full error context.
///
/// [`DisplayErrorContext`] walks the whole error source chain, so the message
/// includes the service error code and message instead of just "service error".
fn backend_error<E>(err: E) -> StorageError
where
    E: std::error::Error,
{
    StorageError::Backend(DisplayErrorContext(&err).to_string())
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
                // Only a typed NoSuchKey means "key absent" — a missing bucket
                // (NoSuchBucket) is also a 404 but must surface as an error.
                if err
                    .as_service_error()
                    .is_some_and(GetObjectError::is_no_such_key)
                {
                    Ok(None)
                } else {
                    Err(backend_error(err))
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
            .map_err(backend_error)?;
        Ok(())
    }

    async fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body));

        if let Some(content_type) = options.content_type {
            request = request.content_type(content_type);
        }

        if !options.metadata.is_empty() {
            request = request.set_metadata(Some(options.metadata));
        }

        request.send().await.map_err(backend_error)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(backend_error)?;
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

        let output = request.send().await.map_err(backend_error)?;

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
                // Only a typed NotFound means "key absent"; other errors
                // (including a missing bucket) surface as backend errors.
                if err
                    .as_service_error()
                    .is_some_and(HeadObjectError::is_not_found)
                {
                    Ok(None)
                } else {
                    Err(backend_error(err))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{size_u64, timestamp_secs};
    use aws_smithy_types::DateTime;

    #[test]
    fn timestamp_secs_converts_positive_epochs() {
        assert_eq!(
            timestamp_secs(&DateTime::from_secs(1_700_000_000)),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn timestamp_secs_rejects_pre_epoch_dates() {
        assert_eq!(timestamp_secs(&DateTime::from_secs(-1)), None);
    }

    #[test]
    fn size_u64_clamps_negative_sizes_to_zero() {
        assert_eq!(size_u64(-5), 0);
        assert_eq!(size_u64(0), 0);
        assert_eq!(size_u64(42), 42);
    }
}
