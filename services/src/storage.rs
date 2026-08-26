//! Object storage abstraction.
//!
//! Provides a platform-agnostic interface for object/blob storage.
//! Implementations include S3, Cloudflare R2, Azure Blob, and in-memory (for testing).

use core::future::Future;
use std::collections::HashMap;

use crate::maybe_send::{BoxFuture, MaybeSend};

// ── Error type ──

/// Errors that can occur during object storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The underlying storage backend returned an error.
    #[error("storage error: {message}")]
    Backend {
        /// A human-readable description of what the backend was asked to do.
        message: String,
        /// The backend's own error, when it hands one back.
        #[source]
        source: Option<crate::BoxError>,
    },

    /// An I/O error occurred.
    #[error("storage I/O error: {0}")]
    Io(String),

    /// The backend does not support the requested operation.
    #[error("unsupported storage operation: {0}")]
    Unsupported(&'static str),

    /// A conditional write failed because the stored object changed underneath it.
    #[error("storage conflict: the object changed before the write was applied")]
    Conflict,

    /// The backend rejected the request because the caller is over its rate limit.
    #[error("storage request was throttled by the backend")]
    Throttled {
        /// How long the backend asked the caller to wait, when it says.
        retry_after: Option<core::time::Duration>,
    },

    /// The configured credentials were rejected by the backend.
    #[error("storage credentials were rejected by the backend")]
    Unauthorized,
}

backend_error!(StorageError);

service_http_error!(StorageError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Io(_) => INTERNAL_SERVER_ERROR,
    Self::Unsupported(_) => NOT_IMPLEMENTED,
    Self::Conflict => CONFLICT,
    Self::Throttled { .. } => TOO_MANY_REQUESTS,
    Self::Unauthorized => INTERNAL_SERVER_ERROR,
});

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

/// Options for storing an object.
#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    /// Content type (MIME) to record with the object.
    pub content_type: Option<String>,
    /// Custom metadata key-value pairs to record with the object.
    pub metadata: HashMap<String, String>,
}

impl PutOptions {
    /// Returns `true` when no option is set, i.e. the put is equivalent to a
    /// plain [`ObjectStorage::put`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content_type.is_none() && self.metadata.is_empty()
    }
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

    /// Store an object under a key with content type and custom metadata.
    ///
    /// The default implementation delegates to [`ObjectStorage::put`] when
    /// `options` is empty and returns [`StorageError::Unsupported`] otherwise,
    /// so backends that cannot record metadata fail loudly instead of
    /// silently dropping it.
    fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> impl Future<Output = Result<(), StorageError>> + MaybeSend {
        async move {
            if options.is_empty() {
                self.put(key, body).await
            } else {
                Err(StorageError::Unsupported(
                    "content type and custom metadata are not supported by this storage backend",
                ))
            }
        }
    }

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
    fn put_with<'a>(
        &'a self,
        key: &'a str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> BoxFuture<'a, Result<(), StorageError>>;
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

    fn put_with<'a>(
        &'a self,
        key: &'a str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> BoxFuture<'a, Result<(), StorageError>> {
        Box::pin(ObjectStorage::put_with(self, key, body, options))
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

service_extractor!(
    Storage,
    StorageNotConfigured,
    "Object storage not configured. Ensure an ObjectStorage implementation is injected."
);

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

    /// Store an object under a key with content type and custom metadata.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the backend operation fails, or
    /// [`StorageError::Unsupported`] if the backend cannot record the
    /// requested options.
    pub async fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        self.0.put_with(key, body, options).await
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

#[cfg(test)]
mod tests {
    use super::{
        ListOptions, ListResult, ObjectMetadata, ObjectStorage, Storage, StorageError,
        StorageObject,
    };
    use http_kit::{Body, Endpoint, HttpError, Middleware, Response};
    use skyzen_core::Extractor;
    use std::{
        collections::HashMap,
        convert::Infallible,
        sync::{Arc, RwLock},
    };

    #[derive(Clone, Default)]
    struct InMemoryObjectStorage {
        data: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    }

    impl InMemoryObjectStorage {
        fn metadata_for(key: &str, body: &[u8]) -> ObjectMetadata {
            ObjectMetadata {
                key: key.to_owned(),
                size: body.len() as u64,
                content_type: None,
                last_modified: None,
                metadata: HashMap::new(),
            }
        }
    }

    impl ObjectStorage for InMemoryObjectStorage {
        async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
            let data = self
                .data
                .read()
                .map_err(|_| StorageError::backend("lock poisoned"))?;
            Ok(data.get(key).map(|body| StorageObject {
                body: body.clone(),
                metadata: Self::metadata_for(key, body),
            }))
        }

        async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
            self.data
                .write()
                .map_err(|_| StorageError::backend("lock poisoned"))?
                .insert(key.to_owned(), body);
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), StorageError> {
            self.data
                .write()
                .map_err(|_| StorageError::backend("lock poisoned"))?
                .remove(key);
            Ok(())
        }

        async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
            let mut objects: Vec<ObjectMetadata> = {
                let data = self
                    .data
                    .read()
                    .map_err(|_| StorageError::backend("lock poisoned"))?;
                data.iter()
                    .filter(|(key, _)| {
                        options
                            .prefix
                            .as_ref()
                            .is_none_or(|prefix| key.starts_with(prefix))
                    })
                    .map(|(key, body)| Self::metadata_for(key, body))
                    .collect()
            };
            objects.sort_by(|left, right| left.key.cmp(&right.key));
            if let Some(limit) = options.limit {
                objects.truncate(limit);
            }

            Ok(ListResult {
                objects,
                cursor: None,
            })
        }

        async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
            let data = self
                .data
                .read()
                .map_err(|_| StorageError::backend("lock poisoned"))?;
            Ok(data.get(key).map(|body| Self::metadata_for(key, body)))
        }
    }

    #[derive(Debug)]
    struct ReadStorageEndpoint;

    impl Endpoint for ReadStorageEndpoint {
        type Error = Infallible;

        async fn respond(
            &mut self,
            request: &mut http_kit::Request,
        ) -> Result<Response, Self::Error> {
            let storage = Storage::extract(request)
                .await
                .expect("storage should be injected");
            let object = storage
                .get("file.txt")
                .await
                .expect("storage access should succeed")
                .expect("object should exist");
            Ok(Response::new(Body::from(object.body)))
        }
    }

    #[tokio::test]
    async fn wrapper_supports_crud_list_and_head_operations() {
        let storage = Storage::new(InMemoryObjectStorage::default());

        storage.put("prefix:a.txt", b"a".to_vec()).await.unwrap();
        storage.put("prefix:b.txt", b"bb".to_vec()).await.unwrap();
        storage.put("other.txt", b"ccc".to_vec()).await.unwrap();

        let object = storage.get("prefix:b.txt").await.unwrap().unwrap();
        assert_eq!(object.body, b"bb".to_vec());
        assert_eq!(object.metadata.size, 2);

        let head = storage.head("prefix:b.txt").await.unwrap().unwrap();
        assert_eq!(head.key, "prefix:b.txt");
        assert_eq!(head.size, 2);

        let listed = storage
            .list(ListOptions {
                prefix: Some("prefix:".to_owned()),
                limit: Some(1),
                cursor: None,
            })
            .await
            .unwrap();
        assert_eq!(listed.objects.len(), 1);
        assert!(listed.objects[0].key.starts_with("prefix:"));

        storage.delete("prefix:a.txt").await.unwrap();
        assert!(storage.get("prefix:a.txt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn middleware_injects_storage_for_downstream_endpoint_and_extractor() {
        let backend = InMemoryObjectStorage::default();
        backend.put("file.txt", b"hello".to_vec()).await.unwrap();
        let mut storage = Storage::new(backend);
        let mut request = http_kit::Request::new(Body::empty());

        let response = storage
            .handle(&mut request, ReadStorageEndpoint)
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "hello");

        let extracted = Storage::extract(&mut request).await.unwrap();
        let object = extracted.get("file.txt").await.unwrap().unwrap();
        assert_eq!(object.body, b"hello".to_vec());
    }

    #[tokio::test]
    async fn extractor_returns_internal_server_error_when_storage_is_missing() {
        let mut request = http_kit::Request::new(Body::empty());

        let error = Storage::extract(&mut request).await.unwrap_err();

        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
