//! Key-value store abstraction.
//!
//! Provides a platform-agnostic interface for key-value storage.
//! Implementations include Redis, Cloudflare KV, `DynamoDB`, and in-memory (for testing).

use core::future::Future;

use serde::{de::DeserializeOwned, Serialize};

use crate::maybe_send::{BoxFuture, MaybeSend};

// ── Error type ──

/// Errors that can occur during key-value store operations.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// The underlying storage backend returned an error.
    #[error("kv store error: {0}")]
    Backend(String),

    /// Serialization or deserialization failed.
    #[error("kv serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The stored value could not be decoded as the requested type.
    #[error("kv decode error: {0}")]
    Decode(String),

    /// The backend does not support the requested operation.
    #[error("unsupported kv operation: {0}")]
    Unsupported(&'static str),
}

// ── Layer 1: Public trait (NOT object-safe, but ergonomic for implementors) ──

/// A platform-agnostic key-value store interface.
///
/// Implementors provide concrete storage backends (Redis, Cloudflare KV, etc.).
/// User code interacts through the [`Kv`] wrapper, never this trait directly.
pub trait KeyValueStore: Send + Sync + Clone + 'static {
    /// Retrieve a value by key.
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, KvError>> + MaybeSend;

    /// Store a value under a key.
    fn put(&self, key: &str, value: &[u8])
        -> impl Future<Output = Result<(), KvError>> + MaybeSend;

    /// Store a value under a key with a time-to-live.
    ///
    /// Once `ttl` elapses the key is treated as absent. Backends without
    /// native expiration return [`KvError::Unsupported`] rather than silently
    /// storing the value forever.
    fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: core::time::Duration,
    ) -> impl Future<Output = Result<(), KvError>> + MaybeSend {
        let _ = (key, value, ttl);
        async {
            Err(KvError::Unsupported(
                "TTL is not supported by this key-value backend",
            ))
        }
    }

    /// Remove a value by key.
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), KvError>> + MaybeSend;

    /// List keys matching an optional prefix.
    fn list(
        &self,
        prefix: Option<&str>,
    ) -> impl Future<Output = Result<Vec<String>, KvError>> + MaybeSend;
}

// ── Layer 2: Private object-safe trait (BoxFuture + clone_box) ──

trait KeyValueStoreObj: Send + Sync {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, KvError>>;
    fn put<'a>(&'a self, key: &'a str, value: &'a [u8]) -> BoxFuture<'a, Result<(), KvError>>;
    fn put_with_ttl<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: core::time::Duration,
    ) -> BoxFuture<'a, Result<(), KvError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), KvError>>;
    fn list<'a>(&'a self, prefix: Option<&'a str>) -> BoxFuture<'a, Result<Vec<String>, KvError>>;
    fn clone_box(&self) -> Box<dyn KeyValueStoreObj>;
}

// ── Bridge: any Clone + KeyValueStore auto-implements KeyValueStoreObj ──

impl<T: KeyValueStore> KeyValueStoreObj for T {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, KvError>> {
        Box::pin(KeyValueStore::get(self, key))
    }

    fn put<'a>(&'a self, key: &'a str, value: &'a [u8]) -> BoxFuture<'a, Result<(), KvError>> {
        Box::pin(KeyValueStore::put(self, key, value))
    }

    fn put_with_ttl<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: core::time::Duration,
    ) -> BoxFuture<'a, Result<(), KvError>> {
        Box::pin(KeyValueStore::put_with_ttl(self, key, value, ttl))
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), KvError>> {
        Box::pin(KeyValueStore::delete(self, key))
    }

    fn list<'a>(&'a self, prefix: Option<&'a str>) -> BoxFuture<'a, Result<Vec<String>, KvError>> {
        Box::pin(KeyValueStore::list(self, prefix))
    }

    fn clone_box(&self) -> Box<dyn KeyValueStoreObj> {
        Box::new(self.clone())
    }
}

// ── User-facing wrapper ──

/// A type-erased key-value store extractor.
///
/// `Kv` wraps any [`KeyValueStore`] implementation behind dynamic dispatch.
/// It is injected into handlers via request extensions and provides
/// convenience methods for JSON and text operations.
///
/// # Example
///
/// ```ignore
/// async fn handler(kv: Kv) -> Result<String> {
///     kv.put("greeting", b"hello").await?;
///     let val = kv.get("greeting").await?;
///     Ok(String::from_utf8_lossy(&val.unwrap_or_default()).into_owned())
/// }
/// ```
pub struct Kv(Box<dyn KeyValueStoreObj>);

service_extractor!(
    Kv,
    KvNotConfigured,
    "KV store not configured. Ensure a KeyValueStore implementation is injected."
);

impl Kv {
    /// Create a new `Kv` from any [`KeyValueStore`] implementation.
    pub fn new(store: impl KeyValueStore) -> Self {
        Self(Box::new(store))
    }

    /// Retrieve raw bytes by key.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        self.0.get(key).await
    }

    /// Store raw bytes under a key.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails.
    pub async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.0.put(key, value).await
    }

    /// Store raw bytes under a key with a time-to-live.
    ///
    /// Once `ttl` elapses the key is treated as absent.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails, or
    /// [`KvError::Unsupported`] if the backend has no native expiration.
    pub async fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: core::time::Duration,
    ) -> Result<(), KvError> {
        self.0.put_with_ttl(key, value, ttl).await
    }

    /// Remove a value by key.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails.
    pub async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.0.delete(key).await
    }

    /// List keys matching an optional prefix.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails.
    pub async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        self.0.list(prefix).await
    }

    /// Retrieve a value and deserialize it from JSON.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails or deserialization fails.
    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, KvError> {
        self.get(key)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()
            .map_err(Into::into)
    }

    /// Retrieve a value as a UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails or the value is not valid UTF-8.
    pub async fn get_text(&self, key: &str) -> Result<Option<String>, KvError> {
        self.get(key).await?.map_or(Ok(None), |bytes| {
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| KvError::Decode(e.to_string()))
        })
    }

    /// Serialize a value to JSON and store it.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if serialization fails or the backend operation fails.
    pub async fn put_json<T: Serialize + Sync>(&self, key: &str, value: &T) -> Result<(), KvError> {
        let bytes = serde_json::to_vec(value)?;
        self.put(key, &bytes).await
    }
}
