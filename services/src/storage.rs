//! Object storage abstraction.
//!
//! Provides a platform-agnostic interface for object/blob storage.
//! Implementations include S3, Cloudflare R2, Azure Blob, and in-memory (for testing).

use core::future::Future;
use std::collections::HashMap;

use http_kit::http_error;
use skyzen_core::{Extractor, StatusCode};

use crate::maybe_send::{BoxFuture, MaybeSend};

// ── Error type ──

/// Errors that can occur during object storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The underlying storage backend returned an error.
    #[error("storage error: {0}")]
    Backend(String),

    /// An I/O error occurred.
    #[error("storage I/O error: {0}")]
    Io(String),
}

// ── Supporting types ──

/// Metadata about a stored object.
#[derive(Debug, Clone)]
pub struct ObjectMetadata {
    /// The object's key (path).
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// Content type (MIME), if known.
    pub content_type: Option<String>,
    /// Last modified timestamp as seconds since Unix epoch.
    pub last_modified: Option<u64>,
    /// Custom metadata key-value pairs.
    pub metadata: HashMap<String, String>,
}

/// A retrieved storage object, including its body and metadata.
#[derive(Debug, Clone)]
pub struct StorageObject {
    /// The raw bytes of the object.
    pub body: Vec<u8>,
    /// Metadata associated with the object.
    pub metadata: ObjectMetadata,
}

/// Options for listing objects.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// Only list objects whose keys start with this prefix.
    pub prefix: Option<String>,
    /// Maximum number of results to return.
    pub limit: Option<usize>,
    /// Continuation token for pagination.
    pub cursor: Option<String>,
}

/// Result of a list operation.
#[derive(Debug, Clone)]
pub struct ListResult {
    /// The objects matching the query.
    pub objects: Vec<ObjectMetadata>,
    /// If present, more results are available; pass this as `cursor` in the next request.
    pub cursor: Option<String>,
}

// ── Layer 1: Public trait ──

/// A platform-agnostic object storage interface.
///
/// Implementors provide concrete storage backends (S3, R2, Azure Blob, etc.).
/// User code interacts through the [`Storage`] wrapper, never this trait directly.
pub trait ObjectStorage: Send + Sync + Clone + 'static {
    /// Retrieve an object by key.
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<StorageObject>, StorageError>> + MaybeSend;

    /// Store an object under a key.
    fn put(
        &self,
        key: &str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<(), StorageError>> + MaybeSend;

    /// Remove an object by key.
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), StorageError>> + MaybeSend;

    /// List objects matching the given options.
    fn list(
        &self,
        options: ListOptions,
    ) -> impl Future<Output = Result<ListResult, StorageError>> + MaybeSend;

    /// Retrieve metadata for an object without downloading the body.
    fn head(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<ObjectMetadata>, StorageError>> + MaybeSend;
}

// ── Layer 2: Private object-safe trait ──

trait ObjectStorageObj: Send + Sync {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<StorageObject>, StorageError>>;
    fn put<'a>(&'a self, key: &'a str, body: Vec<u8>) -> BoxFuture<'a, Result<(), StorageError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StorageError>>;
    fn list(&self, options: ListOptions) -> BoxFuture<'_, Result<ListResult, StorageError>>;
    fn head<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<ObjectMetadata>, StorageError>>;
    fn clone_box(&self) -> Box<dyn ObjectStorageObj>;
}

// ── Bridge ──

impl<T: ObjectStorage> ObjectStorageObj for T {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<StorageObject>, StorageError>> {
        Box::pin(ObjectStorage::get(self, key))
    }

    fn put<'a>(&'a self, key: &'a str, body: Vec<u8>) -> BoxFuture<'a, Result<(), StorageError>> {
        Box::pin(ObjectStorage::put(self, key, body))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), StorageError>> {
        Box::pin(ObjectStorage::delete(self, key))
    }

    fn list(&self, options: ListOptions) -> BoxFuture<'_, Result<ListResult, StorageError>> {
        Box::pin(ObjectStorage::list(self, options))
    }

    fn head<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<ObjectMetadata>, StorageError>> {
        Box::pin(ObjectStorage::head(self, key))
    }

    fn clone_box(&self) -> Box<dyn ObjectStorageObj> {
        Box::new(self.clone())
    }
}

// ── User-facing wrapper ──

/// A type-erased object storage extractor.
///
/// `Storage` wraps any [`ObjectStorage`] implementation behind dynamic dispatch.
/// It is injected into handlers via request extensions.
pub struct Storage(Box<dyn ObjectStorageObj>);

impl Clone for Storage {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Storage").finish_non_exhaustive()
    }
}

impl Storage {
    /// Create a new `Storage` from any [`ObjectStorage`] implementation.
    pub fn new(store: impl ObjectStorage) -> Self {
        Self(Box::new(store))
    }

    /// Retrieve an object by key.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the backend operation fails.
    pub async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        self.0.get(key).await
    }

    /// Store an object under a key.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the backend operation fails.
    pub async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.0.put(key, body).await
    }

    /// Remove an object by key.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the backend operation fails.
    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.0.delete(key).await
    }

    /// List objects matching the given options.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the backend operation fails.
    pub async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        self.0.list(options).await
    }

    /// Retrieve metadata for an object without downloading the body.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the backend operation fails.
    pub async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        self.0.head(key).await
    }
}

http_error!(
    /// The storage service was not found in request extensions.
    pub StorageNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Object storage not configured. Ensure an ObjectStorage implementation is injected."
);

impl Extractor for Storage {
    type Error = StorageNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(StorageNotConfigured::new())
    }
}
