//! Durable Object key-value store abstraction.

use core::future::Future;

use http_kit::http_error;
use serde::{de::DeserializeOwned, Serialize};
use skyzen_core::{Extractor, StatusCode};

use crate::maybe_send::{BoxFuture, MaybeSend};

type DurableKvEntries = Vec<(String, Vec<u8>)>;

// ── Error type ──

/// Errors from Durable Object KV operations.
#[derive(Debug, thiserror::Error)]
pub enum DurableKvError {
    /// The underlying storage backend returned an error.
    #[error("durable kv error: {0}")]
    Backend(String),

    /// Serialization or deserialization failed.
    #[error("durable kv serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Options for listing keys in Durable Object storage.
#[derive(Debug, Default, Clone)]
pub struct DurableListOptions<'a> {
    /// Only return keys starting with this prefix.
    pub prefix: Option<&'a str>,
    /// Start listing after this key (exclusive).
    pub start: Option<&'a str>,
    /// Stop listing at this key (exclusive).
    pub end: Option<&'a str>,
    /// Maximum number of entries to return.
    pub limit: Option<usize>,
    /// Whether to return results in reverse order.
    pub reverse: bool,
}

// ── Layer 1: Public trait ──

/// Durable Object transactional key-value storage.
///
/// Unlike [`KeyValueStore`](crate::kv::KeyValueStore), this operates on
/// the Durable Object's co-located storage with strong consistency.
pub trait DurableKvStore: Send + Sync + Clone + 'static {
    /// Retrieve a value by key.
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, DurableKvError>> + MaybeSend;

    /// Retrieve multiple values by keys.
    fn get_multiple(
        &self,
        keys: &[&str],
    ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + MaybeSend;

    /// Store a value under a key.
    fn put(
        &self,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<(), DurableKvError>> + MaybeSend;

    /// Store multiple key-value pairs atomically.
    fn put_multiple(
        &self,
        entries: &[(&str, &[u8])],
    ) -> impl Future<Output = Result<(), DurableKvError>> + MaybeSend;

    /// Delete a key. Returns whether the key existed.
    fn delete(&self, key: &str) -> impl Future<Output = Result<bool, DurableKvError>> + MaybeSend;

    /// Delete multiple keys. Returns the number of keys deleted.
    fn delete_multiple(
        &self,
        keys: &[&str],
    ) -> impl Future<Output = Result<usize, DurableKvError>> + MaybeSend;

    /// Delete all keys.
    fn delete_all(&self) -> impl Future<Output = Result<(), DurableKvError>> + MaybeSend;

    /// List key-value pairs matching the given options.
    fn list(
        &self,
        options: DurableListOptions<'_>,
    ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + MaybeSend;
}

// ── Layer 2: Private object-safe trait ──

trait DurableKvStoreObj: Send + Sync {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, DurableKvError>>;
    fn get_multiple<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> BoxFuture<'a, Result<DurableKvEntries, DurableKvError>>;
    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
    ) -> BoxFuture<'a, Result<(), DurableKvError>>;
    fn put_multiple<'a>(
        &'a self,
        entries: &'a [(&'a str, &'a [u8])],
    ) -> BoxFuture<'a, Result<(), DurableKvError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool, DurableKvError>>;
    fn delete_multiple<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> BoxFuture<'a, Result<usize, DurableKvError>>;
    fn delete_all(&self) -> BoxFuture<'_, Result<(), DurableKvError>>;
    fn list<'a>(
        &'a self,
        options: DurableListOptions<'a>,
    ) -> BoxFuture<'a, Result<DurableKvEntries, DurableKvError>>;
    fn clone_box(&self) -> Box<dyn DurableKvStoreObj>;
}

// ── Bridge ──

impl<T: DurableKvStore> DurableKvStoreObj for T {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, DurableKvError>> {
        Box::pin(DurableKvStore::get(self, key))
    }
    fn get_multiple<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> BoxFuture<'a, Result<DurableKvEntries, DurableKvError>> {
        Box::pin(DurableKvStore::get_multiple(self, keys))
    }
    fn put<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
    ) -> BoxFuture<'a, Result<(), DurableKvError>> {
        Box::pin(DurableKvStore::put(self, key, value))
    }
    fn put_multiple<'a>(
        &'a self,
        entries: &'a [(&'a str, &'a [u8])],
    ) -> BoxFuture<'a, Result<(), DurableKvError>> {
        Box::pin(DurableKvStore::put_multiple(self, entries))
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool, DurableKvError>> {
        Box::pin(DurableKvStore::delete(self, key))
    }
    fn delete_multiple<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> BoxFuture<'a, Result<usize, DurableKvError>> {
        Box::pin(DurableKvStore::delete_multiple(self, keys))
    }
    fn delete_all(&self) -> BoxFuture<'_, Result<(), DurableKvError>> {
        Box::pin(DurableKvStore::delete_all(self))
    }
    fn list<'a>(
        &'a self,
        options: DurableListOptions<'a>,
    ) -> BoxFuture<'a, Result<DurableKvEntries, DurableKvError>> {
        Box::pin(DurableKvStore::list(self, options))
    }
    fn clone_box(&self) -> Box<dyn DurableKvStoreObj> {
        Box::new(self.clone())
    }
}

// ── User-facing wrapper ──

/// Type-erased Durable Object KV extractor.
///
/// Wraps any [`DurableKvStore`] behind dynamic dispatch and provides
/// convenience methods for JSON operations.
pub struct DurableKv(Box<dyn DurableKvStoreObj>);

impl Clone for DurableKv {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for DurableKv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableKv").finish_non_exhaustive()
    }
}

impl DurableKv {
    /// Create a new `DurableKv` from any [`DurableKvStore`] implementation.
    pub fn new(store: impl DurableKvStore) -> Self {
        Self(Box::new(store))
    }

    /// Retrieve raw bytes by key.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, DurableKvError> {
        self.0.get(key).await
    }

    /// Retrieve multiple values by keys.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn get_multiple(
        &self,
        keys: &[&str],
    ) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        self.0.get_multiple(keys).await
    }

    /// Store raw bytes under a key.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn put(&self, key: &str, value: &[u8]) -> Result<(), DurableKvError> {
        self.0.put(key, value).await
    }

    /// Store multiple key-value pairs atomically.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn put_multiple(&self, entries: &[(&str, &[u8])]) -> Result<(), DurableKvError> {
        self.0.put_multiple(entries).await
    }

    /// Delete a key. Returns whether the key existed.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn delete(&self, key: &str) -> Result<bool, DurableKvError> {
        self.0.delete(key).await
    }

    /// Delete multiple keys. Returns the number of keys deleted.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn delete_multiple(&self, keys: &[&str]) -> Result<usize, DurableKvError> {
        self.0.delete_multiple(keys).await
    }

    /// Delete all keys.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn delete_all(&self) -> Result<(), DurableKvError> {
        self.0.delete_all().await
    }

    /// List key-value pairs matching the given options.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation fails.
    pub async fn list(
        &self,
        options: DurableListOptions<'_>,
    ) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        self.0.list(options).await
    }

    /// Retrieve a value and deserialize it from JSON.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if the backend operation or deserialization fails.
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, DurableKvError> {
        self.get(key)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()
            .map_err(Into::into)
    }

    /// Serialize a value to JSON and store it.
    ///
    /// # Errors
    ///
    /// Returns [`DurableKvError`] if serialization or the backend operation fails.
    pub async fn put_json<T: Serialize + Sync>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<(), DurableKvError> {
        let bytes = serde_json::to_vec(value)?;
        self.put(key, &bytes).await
    }
}

http_error!(
    /// The Durable KV service was not found in request extensions.
    pub DurableKvNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Durable KV store not configured. Ensure a DurableKvStore implementation is injected."
);

impl Extractor for DurableKv {
    type Error = DurableKvNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(DurableKvNotConfigured::new())
    }
}
