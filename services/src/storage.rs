//! Object storage abstraction.
//!
//! Provides a platform-agnostic interface for object/blob storage.
//! Implementations include S3, Cloudflare R2, Azure Blob, and in-memory (for testing).
//!
//! # Whole objects versus byte ranges
//!
//! [`ObjectStorage::get`] and [`ObjectStorage::put`] move an object as one `Vec<u8>`, which is a
//! hard ceiling on an edge runtime: a Cloudflare Worker has about 128 MB of memory, so a video or
//! a database dump cannot round-trip through them. Three additions avoid materializing the whole
//! body — [`get_stream`](ObjectStorage::get_stream) and
//! [`put_stream`](ObjectStorage::put_stream) for chunked transfer,
//! [`get_range`](ObjectStorage::get_range) for serving HTTP `Range` requests, and
//! [`presign_get`](ObjectStorage::presign_get) / [`presign_put`](ObjectStorage::presign_put) for
//! keeping large transfers off the application server entirely.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use std::collections::HashMap;

use bytes::Bytes;
use futures_core::Stream;
use http_kit::{
    header::{HeaderName, HeaderValue},
    Method,
};

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
    ///
    /// Always the size of the whole object, including on a
    /// [`get_range`](ObjectStorage::get_range) result whose body holds only the requested slice —
    /// that is what a handler needs to render a `Content-Range`.
    pub size: u64,
    /// Content type (MIME), if known.
    pub content_type: Option<String>,
    /// Last modified timestamp as seconds since Unix epoch.
    pub last_modified: Option<u64>,
    /// Custom metadata key-value pairs.
    pub metadata: HashMap<String, String>,
    /// The object's entity tag in the quoted HTTP form (`"d41d8cd9…"`), when the backend reports
    /// one.
    ///
    /// Quoted because that is the form an `ETag` / `If-None-Match` header carries, so a handler can
    /// pass it straight through. Backends that natively hand back a bare digest (R2's `etag`) fill
    /// this from their HTTP-shaped field (R2's `httpEtag`) instead.
    pub etag: Option<String>,
    /// The backend's own version identifier for this object, when it versions objects.
    pub version: Option<String>,
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
///
/// `content_type` and `metadata` are the two every backend records. The rest are *extended*
/// options — generic object-storage concepts (S3, R2 and Azure Blob all have them) that an
/// individual backend may not be wired for yet. A backend never drops one silently: it honours
/// what it can and hands the rest to [`reject_unsupported`](Self::reject_unsupported).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PutOptions {
    /// Content type (MIME) to record with the object.
    pub content_type: Option<String>,
    /// Custom metadata key-value pairs to record with the object.
    pub metadata: HashMap<String, String>,
    /// `Cache-Control` to serve the object with.
    pub cache_control: Option<String>,
    /// `Content-Encoding` to serve the object with.
    pub content_encoding: Option<String>,
    /// `Content-Disposition` to serve the object with.
    pub content_disposition: Option<String>,
    /// The backend's storage tier for this object (S3's `STANDARD`/`GLACIER`, R2's
    /// `Standard`/`InfrequentAccess`). Spelled the way the backend spells it.
    pub storage_class: Option<String>,
    /// The raw MD5 digest of `body`, for the backend to verify the upload against.
    ///
    /// Raw digest bytes, not hex and not base64: each backend encodes them the way its own API
    /// wants. A mismatch is the backend's error, which is the point of sending it.
    pub content_md5: Option<Vec<u8>>,
}

/// One extended [`PutOptions`] field — everything past content type and custom metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PutOption {
    /// [`PutOptions::cache_control`].
    CacheControl,
    /// [`PutOptions::content_encoding`].
    ContentEncoding,
    /// [`PutOptions::content_disposition`].
    ContentDisposition,
    /// [`PutOptions::storage_class`].
    StorageClass,
    /// [`PutOptions::content_md5`].
    ContentMd5,
}

impl PutOption {
    /// Every extended option, in the order [`PutOptions`] declares them.
    pub const ALL: [Self; 5] = [
        Self::CacheControl,
        Self::ContentEncoding,
        Self::ContentDisposition,
        Self::StorageClass,
        Self::ContentMd5,
    ];

    /// What [`PutOptions::reject_unsupported`] reports when a backend cannot honour this option.
    const fn unsupported(self) -> &'static str {
        match self {
            Self::CacheControl => "cache control is not supported by this storage backend",
            Self::ContentEncoding => "content encoding is not supported by this storage backend",
            Self::ContentDisposition => {
                "content disposition is not supported by this storage backend"
            }
            Self::StorageClass => "storage classes are not supported by this storage backend",
            Self::ContentMd5 => "upload checksums are not supported by this storage backend",
        }
    }
}

impl PutOptions {
    /// Options that record nothing beyond the body.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `content_type` as the object's MIME type.
    #[must_use]
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Record one custom metadata entry alongside the object.
    #[must_use]
    pub fn with_metadata(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(name.into(), value.into());
        self
    }

    /// Serve the object with this `Cache-Control`.
    #[must_use]
    pub fn with_cache_control(mut self, cache_control: impl Into<String>) -> Self {
        self.cache_control = Some(cache_control.into());
        self
    }

    /// Serve the object with this `Content-Encoding`.
    #[must_use]
    pub fn with_content_encoding(mut self, content_encoding: impl Into<String>) -> Self {
        self.content_encoding = Some(content_encoding.into());
        self
    }

    /// Serve the object with this `Content-Disposition`.
    #[must_use]
    pub fn with_content_disposition(mut self, content_disposition: impl Into<String>) -> Self {
        self.content_disposition = Some(content_disposition.into());
        self
    }

    /// Store the object in this backend-named storage tier.
    #[must_use]
    pub fn with_storage_class(mut self, storage_class: impl Into<String>) -> Self {
        self.storage_class = Some(storage_class.into());
        self
    }

    /// Have the backend verify the upload against this raw MD5 digest.
    #[must_use]
    pub fn with_content_md5(mut self, digest: impl Into<Vec<u8>>) -> Self {
        self.content_md5 = Some(digest.into());
        self
    }

    /// Returns `true` when no option is set, i.e. the put is equivalent to a
    /// plain [`ObjectStorage::put`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content_type.is_none() && self.metadata.is_empty() && self.extended().next().is_none()
    }

    /// Which extended options this request actually sets.
    pub fn extended(&self) -> impl Iterator<Item = PutOption> + '_ {
        PutOption::ALL.into_iter().filter(|option| match option {
            PutOption::CacheControl => self.cache_control.is_some(),
            PutOption::ContentEncoding => self.content_encoding.is_some(),
            PutOption::ContentDisposition => self.content_disposition.is_some(),
            PutOption::StorageClass => self.storage_class.is_some(),
            PutOption::ContentMd5 => self.content_md5.is_some(),
        })
    }

    /// Fail when this request sets an extended option the backend does not honour.
    ///
    /// `honoured` names the options the caller has actually wired into its request; anything else
    /// that is set becomes a [`StorageError::Unsupported`] instead of being dropped on the way to
    /// the backend, so a caller that asked for `Cache-Control` never gets a silent success from a
    /// backend that ignored it.
    ///
    /// # Errors
    ///
    /// [`StorageError::Unsupported`] naming the first set option that is not in `honoured`.
    pub fn reject_unsupported(&self, honoured: &[PutOption]) -> Result<(), StorageError> {
        self.extended()
            .find(|option| !honoured.contains(option))
            .map_or(Ok(()), |option| {
                Err(StorageError::Unsupported(option.unsupported()))
            })
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

/// A chunked object body.
///
/// Wraps a boxed `Stream` of [`Bytes`] so neither the trait nor the [`Storage`] wrapper has to
/// name a concrete stream type, and so a body can cross the type-erased service boundary without
/// being buffered. `StorageStream` itself implements [`Stream`], so it can be forwarded straight
/// into a response body.
pub struct StorageStream(futures_core::stream::BoxStream<'static, Result<Bytes, StorageError>>);

impl core::fmt::Debug for StorageStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StorageStream").finish_non_exhaustive()
    }
}

impl StorageStream {
    /// Wrap any `Send` stream of byte chunks.
    pub fn new(stream: impl Stream<Item = Result<Bytes, StorageError>> + Send + 'static) -> Self {
        Self(Box::pin(stream))
    }

    /// A stream that yields one already-buffered chunk.
    ///
    /// Useful for backends that have no chunked API of their own but should still satisfy
    /// [`ObjectStorage::get_stream`].
    pub fn once(chunk: impl Into<Bytes>) -> Self {
        Self::new(Once(Some(chunk.into())))
    }

    /// Drain the stream into one contiguous buffer.
    ///
    /// This gives up the whole point of streaming, so use it only where the body is known to be
    /// small — a mock, a test assertion, or a backend whose write API takes a full buffer.
    ///
    /// # Errors
    ///
    /// Returns whatever [`StorageError`] the stream yields.
    pub async fn into_bytes(mut self) -> Result<Vec<u8>, StorageError> {
        let mut buffer = Vec::new();
        while let Some(chunk) = core::future::poll_fn(|cx| Pin::new(&mut self).poll_next(cx)).await
        {
            buffer.extend_from_slice(&chunk?);
        }
        Ok(buffer)
    }
}

impl Stream for StorageStream {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().0.as_mut().poll_next(cx)
    }
}

/// A stream yielding a single pre-built chunk.
struct Once(Option<Bytes>);

impl Stream for Once {
    type Item = Result<Bytes, StorageError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.take().map(Ok))
    }
}

/// The slice of an object to read.
///
/// Mirrors the two forms an HTTP `Range` header can take, so a handler can pass a parsed request
/// range straight through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// `length` bytes starting at `offset`, or everything from `offset` when `length` is `None`.
    FromStart {
        /// Zero-based index of the first byte to read.
        offset: u64,
        /// How many bytes to read; `None` reads to the end of the object.
        length: Option<u64>,
    },
    /// The last N bytes of the object (HTTP's `Range: bytes=-N`).
    Suffix(u64),
}

impl ByteRange {
    /// Everything from `offset` to the end of the object.
    #[must_use]
    pub const fn from_start(offset: u64) -> Self {
        Self::FromStart {
            offset,
            length: None,
        }
    }

    /// `length` bytes starting at `offset`.
    #[must_use]
    pub const fn slice(offset: u64, length: u64) -> Self {
        Self::FromStart {
            offset,
            length: Some(length),
        }
    }

    /// The last `length` bytes of the object.
    #[must_use]
    pub const fn suffix(length: u64) -> Self {
        Self::Suffix(length)
    }

    /// Resolve the range against an object of `total` bytes, returning `start..end`.
    ///
    /// Returns `None` when the range selects nothing — an offset at or past the end, or a
    /// zero-length request — which is the case a backend reports as an unsatisfiable range rather
    /// than as an empty success. A range that runs past the end is clamped, as HTTP requires.
    #[must_use]
    pub const fn resolve(self, total: u64) -> Option<core::ops::Range<u64>> {
        let (start, end) = match self {
            Self::FromStart { offset, length } => {
                if offset >= total {
                    return None;
                }
                let end = match length {
                    Some(length) => {
                        let requested = offset.saturating_add(length);
                        if requested < total {
                            requested
                        } else {
                            total
                        }
                    }
                    None => total,
                };
                (offset, end)
            }
            Self::Suffix(length) => {
                if length == 0 {
                    return None;
                }
                (total.saturating_sub(length), total)
            }
        };

        if start >= end {
            None
        } else {
            Some(start..end)
        }
    }
}

/// A pre-authorized request a client can issue directly against the storage backend.
///
/// Handing one of these to a browser keeps a large upload or download off the application server
/// entirely, which is the normal way to move big objects on S3, R2 and Azure Blob.
#[derive(Debug, Clone)]
pub struct PresignedRequest {
    /// The URL to issue the request against; the authorization is embedded in it.
    pub url: String,
    /// The HTTP method the signature covers. Using another method invalidates it.
    pub method: Method,
    /// Headers the client must send verbatim for the signature to verify.
    pub headers: Vec<(HeaderName, HeaderValue)>,
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
    ) -> impl Future<Output = Result<Option<StorageObject>, StorageError>> + Send;

    /// Store an object under a key.
    fn put(
        &self,
        key: &str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

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
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
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
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// List objects matching the given options.
    fn list(
        &self,
        options: ListOptions,
    ) -> impl Future<Output = Result<ListResult, StorageError>> + Send;

    /// Retrieve metadata for an object without downloading the body.
    fn head(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<ObjectMetadata>, StorageError>> + Send;

    /// Retrieve an object's body as a stream of chunks.
    ///
    /// `None` means the object does not exist. Backends without a chunked read return
    /// [`StorageError::Unsupported`] rather than quietly buffering the whole object, which would
    /// defeat the point of asking for a stream.
    fn get_stream(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<StorageStream>, StorageError>> + Send {
        let _ = key;
        async {
            Err(StorageError::Unsupported(
                "streaming reads are not supported by this storage backend",
            ))
        }
    }

    /// Store an object from a stream of chunks.
    ///
    /// `content_length` is the total size when the caller knows it; backends that require a length
    /// up front (or that would otherwise have to buffer to discover it) may reject a `None`.
    fn put_stream(
        &self,
        key: &str,
        stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        let _ = (key, stream, content_length, options);
        async {
            Err(StorageError::Unsupported(
                "streaming writes are not supported by this storage backend",
            ))
        }
    }

    /// Retrieve part of an object.
    ///
    /// The returned [`StorageObject::body`] holds only the requested slice, while
    /// [`ObjectMetadata::size`] keeps reporting the size of the whole object, so a handler can
    /// build a `Content-Range` from both. `None` means the object does not exist; a range that
    /// selects nothing is [`StorageError::Unsupported`]'s neighbour rather than an empty success —
    /// see [`ByteRange::resolve`].
    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Option<StorageObject>, StorageError>> + Send {
        let _ = (key, range);
        async {
            Err(StorageError::Unsupported(
                "ranged reads are not supported by this storage backend",
            ))
        }
    }

    /// Mint a pre-authorized request a client can use to download the object directly.
    fn presign_get(
        &self,
        key: &str,
        expires_in: core::time::Duration,
    ) -> impl Future<Output = Result<PresignedRequest, StorageError>> + Send {
        let _ = (key, expires_in);
        async {
            Err(StorageError::Unsupported(
                "presigned URLs are not supported by this storage backend",
            ))
        }
    }

    /// Mint a pre-authorized request a client can use to upload the object directly.
    fn presign_put(
        &self,
        key: &str,
        expires_in: core::time::Duration,
        options: PutOptions,
    ) -> impl Future<Output = Result<PresignedRequest, StorageError>> + Send {
        let _ = (key, expires_in, options);
        async {
            Err(StorageError::Unsupported(
                "presigned URLs are not supported by this storage backend",
            ))
        }
    }
}

// ── Layer 2: Generated object-safe trait ──

service_obj! {
    ObjectStorageObj: ObjectStorage;
    async fn get<'a>(&'a self, key: &'a str) -> Result<Option<StorageObject>, StorageError>;
    async fn put<'a>(&'a self, key: &'a str, body: Vec<u8>) -> Result<(), StorageError>;
    async fn put_with<'a>(
        &'a self,
        key: &'a str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError>;
    async fn delete<'a>(&'a self, key: &'a str) -> Result<(), StorageError>;
    async fn list(&'_ self, options: ListOptions) -> Result<ListResult, StorageError>;
    async fn head<'a>(&'a self, key: &'a str) -> Result<Option<ObjectMetadata>, StorageError>;
    async fn get_stream<'a>(&'a self, key: &'a str) -> Result<Option<StorageStream>, StorageError>;
    async fn put_stream<'a>(
        &'a self,
        key: &'a str,
        stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> Result<(), StorageError>;
    async fn get_range<'a>(
        &'a self,
        key: &'a str,
        range: ByteRange,
    ) -> Result<Option<StorageObject>, StorageError>;
    async fn presign_get<'a>(
        &'a self,
        key: &'a str,
        expires_in: core::time::Duration,
    ) -> Result<PresignedRequest, StorageError>;
    async fn presign_put<'a>(
        &'a self,
        key: &'a str,
        expires_in: core::time::Duration,
        options: PutOptions,
    ) -> Result<PresignedRequest, StorageError>;
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

    /// Retrieve an object's body as a stream of chunks, without buffering the whole object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Unsupported`] if the backend has no chunked read, or another
    /// [`StorageError`] if the backend operation fails.
    pub async fn get_stream(&self, key: &str) -> Result<Option<StorageStream>, StorageError> {
        self.0.get_stream(key).await
    }

    /// Store an object from a stream of chunks.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Unsupported`] if the backend has no chunked write, or another
    /// [`StorageError`] if the backend operation fails.
    pub async fn put_stream(
        &self,
        key: &str,
        stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        self.0
            .put_stream(key, stream, content_length, options)
            .await
    }

    /// Retrieve part of an object, for answering an HTTP `Range` request.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Unsupported`] if the backend has no ranged read, or another
    /// [`StorageError`] if the backend operation fails.
    pub async fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> Result<Option<StorageObject>, StorageError> {
        self.0.get_range(key, range).await
    }

    /// Mint a pre-authorized request a client can use to download the object directly.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Unsupported`] if the backend cannot presign, or another
    /// [`StorageError`] if the backend operation fails.
    pub async fn presign_get(
        &self,
        key: &str,
        expires_in: core::time::Duration,
    ) -> Result<PresignedRequest, StorageError> {
        self.0.presign_get(key, expires_in).await
    }

    /// Mint a pre-authorized request a client can use to upload the object directly.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Unsupported`] if the backend cannot presign, or another
    /// [`StorageError`] if the backend operation fails.
    pub async fn presign_put(
        &self,
        key: &str,
        expires_in: core::time::Duration,
        options: PutOptions,
    ) -> Result<PresignedRequest, StorageError> {
        self.0.presign_put(key, expires_in, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ByteRange, ListOptions, ListResult, ObjectMetadata, ObjectStorage, Storage, StorageError,
        StorageObject, StorageStream,
    };
    use http_kit::{Body, Endpoint, HttpError, Response};
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
                etag: None,
                version: None,
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

    #[derive(Debug, Clone)]
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
        let storage = Storage::new(backend);
        let mut request = http_kit::Request::new(Body::empty());

        let response =
            ::skyzen_core::middleware::apply(&storage, &mut request, ReadStorageEndpoint)
                .await
                .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "hello");

        let extracted = Storage::extract(&mut request).await.unwrap();
        let object = extracted.get("file.txt").await.unwrap().unwrap();
        assert_eq!(object.body, b"hello".to_vec());
    }

    #[test]
    fn byte_range_resolves_against_a_known_object_size() {
        assert_eq!(ByteRange::slice(2, 3).resolve(10), Some(2..5));
        // A range that runs past the end is clamped, as HTTP requires.
        assert_eq!(ByteRange::slice(8, 100).resolve(10), Some(8..10));
        assert_eq!(ByteRange::from_start(4).resolve(10), Some(4..10));
        assert_eq!(ByteRange::suffix(3).resolve(10), Some(7..10));
        // A suffix longer than the object is the whole object.
        assert_eq!(ByteRange::suffix(50).resolve(10), Some(0..10));
    }

    #[test]
    fn byte_range_reports_an_unsatisfiable_selection_as_none() {
        assert_eq!(ByteRange::from_start(10).resolve(10), None);
        assert_eq!(ByteRange::slice(3, 0).resolve(10), None);
        assert_eq!(ByteRange::suffix(0).resolve(10), None);
        assert_eq!(ByteRange::from_start(0).resolve(0), None);
    }

    #[tokio::test]
    async fn storage_stream_round_trips_a_buffered_chunk() {
        let stream = StorageStream::once(b"hello".to_vec());
        assert_eq!(stream.into_bytes().await.unwrap(), b"hello".to_vec());
    }

    #[tokio::test]
    async fn streaming_range_and_presign_default_to_unsupported() {
        let storage = Storage::new(InMemoryObjectStorage::default());

        assert!(matches!(
            storage.get_stream("file.txt").await.unwrap_err(),
            StorageError::Unsupported(_)
        ));
        assert!(matches!(
            storage
                .put_stream(
                    "file.txt",
                    StorageStream::once(b"data".to_vec()),
                    Some(4),
                    super::PutOptions::default(),
                )
                .await
                .unwrap_err(),
            StorageError::Unsupported(_)
        ));
        assert!(matches!(
            storage
                .get_range("file.txt", ByteRange::slice(0, 1))
                .await
                .unwrap_err(),
            StorageError::Unsupported(_)
        ));
        assert!(matches!(
            storage
                .presign_get("file.txt", core::time::Duration::from_mins(15))
                .await
                .unwrap_err(),
            StorageError::Unsupported(_)
        ));
        assert!(matches!(
            storage
                .presign_put(
                    "file.txt",
                    core::time::Duration::from_mins(15),
                    super::PutOptions::default(),
                )
                .await
                .unwrap_err(),
            StorageError::Unsupported(_)
        ));
    }

    #[test]
    fn put_options_report_the_extended_options_a_backend_cannot_honour() {
        use super::{PutOption, PutOptions};

        let plain = PutOptions::new()
            .with_content_type("text/plain")
            .with_metadata("owner", "tests");
        assert!(!plain.is_empty());
        assert_eq!(plain.extended().count(), 0);
        plain
            .reject_unsupported(&[])
            .expect("content type and metadata are honoured everywhere");

        let extended = PutOptions::new()
            .with_cache_control("public, max-age=60")
            .with_storage_class("InfrequentAccess");
        assert_eq!(
            extended.extended().collect::<Vec<_>>(),
            vec![PutOption::CacheControl, PutOption::StorageClass]
        );
        // A backend that wires up only one of the two still refuses the other.
        assert!(matches!(
            extended
                .reject_unsupported(&[PutOption::CacheControl])
                .unwrap_err(),
            StorageError::Unsupported(_)
        ));
        extended
            .reject_unsupported(&PutOption::ALL)
            .expect("a backend honouring everything accepts everything");

        assert!(PutOptions::new().is_empty());
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
