//! Key-value store abstraction.
//!
//! Provides a platform-agnostic interface for key-value storage.
//! Implementations include Redis, Cloudflare KV, `DynamoDB`, and in-memory (for testing).

use core::future::Future;

use serde::{de::DeserializeOwned, Serialize};

// ── Error type ──

/// Errors that can occur during key-value store operations.
#[derive(Debug, thiserror::Error)]
pub enum KvError {
    /// The underlying storage backend returned an error.
    #[error("kv store error: {message}")]
    Backend {
        /// A human-readable description of what the backend was asked to do.
        message: String,
        /// The backend's own error, when it hands one back.
        #[source]
        source: Option<crate::BoxError>,
    },

    /// Serialization or deserialization failed.
    #[error("kv serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The stored value could not be decoded as the requested type.
    #[error("kv decode error: {0}")]
    Decode(String),

    /// The backend does not support the requested operation.
    #[error("unsupported kv operation: {0}")]
    Unsupported(&'static str),

    /// A conditional write failed because the stored value changed underneath it.
    #[error("kv conflict: the stored value changed before the write was applied")]
    Conflict,

    /// The backend rejected the request because the caller is over its rate limit.
    #[error("kv request was throttled by the backend")]
    Throttled {
        /// How long the backend asked the caller to wait, when it says.
        retry_after: Option<core::time::Duration>,
    },

    /// The configured credentials were rejected by the backend.
    #[error("kv credentials were rejected by the backend")]
    Unauthorized,
}

backend_error!(KvError);

service_http_error!(KvError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Serialization(_) => INTERNAL_SERVER_ERROR,
    Self::Decode(_) => INTERNAL_SERVER_ERROR,
    Self::Unsupported(_) => NOT_IMPLEMENTED,
    Self::Conflict => CONFLICT,
    Self::Throttled { .. } => TOO_MANY_REQUESTS,
    Self::Unauthorized => INTERNAL_SERVER_ERROR,
});

// ── Supporting types ──

/// Options for one page of a [`KeyValueStore::list`] scan.
///
/// Mirrors [`ListOptions`](crate::storage::ListOptions) so the KV, object-storage and Durable
/// Object listings read the same way.
#[derive(Debug, Clone, Default)]
pub struct KvListOptions {
    /// Only list keys that start with this prefix.
    pub prefix: Option<String>,
    /// Maximum number of keys to return in this page. `None` lets the backend pick its own page
    /// size, which is **not** the same as "every key": follow the returned cursor to see the rest.
    pub limit: Option<usize>,
    /// Continuation token from the previous page's [`KvListResult::cursor`].
    pub cursor: Option<String>,
}

impl KvListOptions {
    /// Options that list every key from the beginning, one backend page at a time.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prefix: None,
            limit: None,
            cursor: None,
        }
    }

    /// Restrict the listing to keys starting with `prefix`.
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Cap the page at `limit` keys.
    #[must_use]
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Resume from the cursor returned by a previous page.
    #[must_use]
    pub fn with_cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }
}

/// One page of keys returned by [`KeyValueStore::list`].
#[derive(Debug, Clone, Default)]
pub struct KvListResult {
    /// The keys in this page.
    pub keys: Vec<String>,
    /// Present when more keys remain; pass it back as [`KvListOptions::cursor`].
    pub cursor: Option<String>,
}

// ── Layer 1: Public trait (NOT object-safe, but ergonomic for implementors) ──

/// A platform-agnostic key-value store interface.
///
/// Implementors provide concrete storage backends (Redis, Cloudflare KV, etc.).
/// User code interacts through the [`Kv`] wrapper, never this trait directly.
pub trait KeyValueStore: Send + Sync + Clone + 'static {
    /// Retrieve a value by key.
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, KvError>> + Send;

    /// Store a value under a key.
    fn put(&self, key: &str, value: &[u8]) -> impl Future<Output = Result<(), KvError>> + Send;

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
    ) -> impl Future<Output = Result<(), KvError>> + Send {
        let _ = (key, value, ttl);
        async {
            Err(KvError::Unsupported(
                "TTL is not supported by this key-value backend",
            ))
        }
    }

    /// Store a value only if the key is currently absent.
    ///
    /// Returns `true` when the value was written and `false` when the key already held one, which
    /// is what makes it usable as a distributed lock or an idempotency key. Backends without a
    /// conditional write return [`KvError::Unsupported`] rather than degrading to a blind
    /// last-writer-wins [`put`](KeyValueStore::put).
    fn put_if_absent(
        &self,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<bool, KvError>> + Send {
        let _ = (key, value);
        async {
            Err(KvError::Unsupported(
                "conditional writes are not supported by this key-value backend",
            ))
        }
    }

    /// Replace the value under `key` only if it still matches `expected`.
    ///
    /// `expected` is `None` to require that the key be absent. Returns `true` when the swap was
    /// applied and `false` when the precondition no longer held.
    ///
    /// A lost race is an ordinary outcome of an optimistic update, so it is reported as
    /// `Ok(false)`, never as an error: [`KvError::Conflict`] is reserved for conflicts a backend
    /// raises on its own, and `compare_and_swap` never produces it for a plain mismatch.
    fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: &[u8],
    ) -> impl Future<Output = Result<bool, KvError>> + Send {
        let _ = (key, expected, new);
        async {
            Err(KvError::Unsupported(
                "compare-and-swap is not supported by this key-value backend",
            ))
        }
    }

    /// Atomically add `delta` to the counter stored under `key` and return the new value.
    ///
    /// An absent key counts as zero, so the first `increment(key, 1)` yields `1`. Backends without
    /// an atomic counter return [`KvError::Unsupported`] instead of racing a read-modify-write.
    fn increment(
        &self,
        key: &str,
        delta: i64,
    ) -> impl Future<Output = Result<i64, KvError>> + Send {
        let _ = (key, delta);
        async {
            Err(KvError::Unsupported(
                "atomic counters are not supported by this key-value backend",
            ))
        }
    }

    /// Set or refresh the time-to-live of a key that already holds a value.
    ///
    /// Returns `false` when no such key exists. This is what sliding-expiration sessions need, and
    /// what [`put_with_ttl`](KeyValueStore::put_with_ttl) alone cannot express.
    fn expire(
        &self,
        key: &str,
        ttl: core::time::Duration,
    ) -> impl Future<Output = Result<bool, KvError>> + Send {
        let _ = (key, ttl);
        async {
            Err(KvError::Unsupported(
                "refreshing a key's TTL is not supported by this key-value backend",
            ))
        }
    }

    /// Report whether a key currently holds a value.
    ///
    /// The default reads the value through [`get`](KeyValueStore::get) and discards it, which is
    /// correct everywhere but pays for the whole payload; backends with a native existence check
    /// (Redis `EXISTS`, an S3 `HEAD`) should override it.
    fn exists(&self, key: &str) -> impl Future<Output = Result<bool, KvError>> + Send {
        async move { Ok(self.get(key).await?.is_some()) }
    }

    /// Remove a value by key.
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), KvError>> + Send;

    /// List one page of keys.
    ///
    /// Backends return at most [`KvListOptions::limit`] keys and, when more remain, a
    /// [`KvListResult::cursor`] to resume from. Pagination is the backend's native one wherever it
    /// has one (Redis `SCAN`, `DynamoDB`'s `ExclusiveStartKey`, Cloudflare KV's list cursor), so a
    /// large namespace never has to be materialized at once — use
    /// [`Kv::list_all`] when you really do want every key.
    fn list(
        &self,
        options: KvListOptions,
    ) -> impl Future<Output = Result<KvListResult, KvError>> + Send;
}

// ── Layer 2: Generated object-safe trait (BoxFuture + clone_box) ──

service_obj! {
    KeyValueStoreObj: KeyValueStore;
    async fn get<'a>(&'a self, key: &'a str) -> Result<Option<Vec<u8>>, KvError>;
    async fn put<'a>(&'a self, key: &'a str, value: &'a [u8]) -> Result<(), KvError>;
    async fn put_with_ttl<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: core::time::Duration,
    ) -> Result<(), KvError>;
    async fn put_if_absent<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
    ) -> Result<bool, KvError>;
    async fn compare_and_swap<'a>(
        &'a self,
        key: &'a str,
        expected: Option<&'a [u8]>,
        new: &'a [u8],
    ) -> Result<bool, KvError>;
    async fn increment<'a>(&'a self, key: &'a str, delta: i64) -> Result<i64, KvError>;
    async fn expire<'a>(
        &'a self,
        key: &'a str,
        ttl: core::time::Duration,
    ) -> Result<bool, KvError>;
    async fn exists<'a>(&'a self, key: &'a str) -> Result<bool, KvError>;
    async fn delete<'a>(&'a self, key: &'a str) -> Result<(), KvError>;
    async fn list(&'_ self, options: KvListOptions) -> Result<KvListResult, KvError>;
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

    /// Store raw bytes only if the key is currently absent.
    ///
    /// Returns `true` when the value was written and `false` when the key already held one.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Unsupported`] if the backend has no conditional write, or another
    /// [`KvError`] if the backend operation fails.
    pub async fn put_if_absent(&self, key: &str, value: &[u8]) -> Result<bool, KvError> {
        self.0.put_if_absent(key, value).await
    }

    /// Replace the value under `key` only if it still matches `expected`.
    ///
    /// `expected` is `None` to require that the key be absent. `Ok(false)` means the precondition
    /// no longer held — an ordinary outcome of an optimistic update, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Unsupported`] if the backend has no compare-and-swap, or another
    /// [`KvError`] if the backend operation fails.
    pub async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: &[u8],
    ) -> Result<bool, KvError> {
        self.0.compare_and_swap(key, expected, new).await
    }

    /// Atomically add `delta` to the counter stored under `key` and return the new value.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Unsupported`] if the backend has no atomic counter, [`KvError::Decode`]
    /// if the stored value is not an integer, or another [`KvError`] if the backend fails.
    pub async fn increment(&self, key: &str, delta: i64) -> Result<i64, KvError> {
        self.0.increment(key, delta).await
    }

    /// Set or refresh the time-to-live of a key that already holds a value.
    ///
    /// Returns `false` when no such key exists.
    ///
    /// # Errors
    ///
    /// Returns [`KvError::Unsupported`] if the backend cannot re-arm a TTL, or another
    /// [`KvError`] if the backend operation fails.
    pub async fn expire(&self, key: &str, ttl: core::time::Duration) -> Result<bool, KvError> {
        self.0.expire(key, ttl).await
    }

    /// Report whether a key currently holds a value.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails.
    pub async fn exists(&self, key: &str) -> Result<bool, KvError> {
        self.0.exists(key).await
    }

    /// Remove a value by key.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails.
    pub async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.0.delete(key).await
    }

    /// List one page of keys.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails.
    pub async fn list(&self, options: KvListOptions) -> Result<KvListResult, KvError> {
        self.0.list(options).await
    }

    /// List **every** key matching `prefix`, following the backend's cursor until it is exhausted.
    ///
    /// This can be expensive: it holds the whole key set in memory and issues one request per
    /// page, and on `DynamoDB` it is a full table scan. Prefer [`list`](Self::list) with a limit
    /// for anything user-facing.
    ///
    /// # Errors
    ///
    /// Returns [`KvError`] if the backend operation fails, or [`KvError::Backend`] if the backend
    /// repeats a cursor instead of advancing, which would otherwise loop forever.
    pub async fn list_all(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let mut keys = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let page = self
                .list(KvListOptions {
                    prefix: prefix.map(ToOwned::to_owned),
                    limit: None,
                    cursor: cursor.clone(),
                })
                .await?;
            keys.extend(page.keys);

            match page.cursor {
                None => return Ok(keys),
                Some(next) if Some(&next) == cursor.as_ref() => {
                    return Err(KvError::backend(format!(
                        "the backend repeated list cursor {next:?} instead of advancing"
                    )));
                }
                Some(next) => cursor = Some(next),
            }
        }
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

#[cfg(test)]
mod tests {
    use super::{KeyValueStore, Kv, KvError, KvListOptions, KvListResult};
    use http_kit::HttpError;
    use serde::Deserialize;
    use skyzen_core::Extractor;
    use std::{
        collections::HashMap,
        sync::{Arc, RwLock},
    };

    #[derive(Clone, Default)]
    struct InMemoryKeyValueStore {
        data: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    }

    impl KeyValueStore for InMemoryKeyValueStore {
        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
            let data = self
                .data
                .read()
                .map_err(|_| KvError::backend("lock poisoned"))?;
            Ok(data.get(key).cloned())
        }

        async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
            self.data
                .write()
                .map_err(|_| KvError::backend("lock poisoned"))?
                .insert(key.to_owned(), value.to_vec());
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), KvError> {
            self.data
                .write()
                .map_err(|_| KvError::backend("lock poisoned"))?
                .remove(key);
            Ok(())
        }

        async fn list(&self, options: KvListOptions) -> Result<KvListResult, KvError> {
            let mut keys: Vec<String> = {
                let data = self
                    .data
                    .read()
                    .map_err(|_| KvError::backend("lock poisoned"))?;
                data.keys()
                    .filter(|key| {
                        options
                            .prefix
                            .as_ref()
                            .is_none_or(|prefix| key.starts_with(prefix))
                            && options
                                .cursor
                                .as_ref()
                                .is_none_or(|cursor| key.as_str() > cursor.as_str())
                    })
                    .cloned()
                    .collect()
            };
            keys.sort();

            let mut cursor = None;
            if let Some(limit) = options.limit {
                if keys.len() > limit {
                    keys.truncate(limit);
                    cursor = keys.last().cloned();
                }
            }

            Ok(KvListResult { keys, cursor })
        }
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Greeting {
        message: String,
    }

    #[tokio::test]
    async fn wrapper_supports_crud_and_list_round_trip() {
        let kv = Kv::new(InMemoryKeyValueStore::default());

        kv.put("prefix:a", b"one").await.unwrap();
        kv.put("prefix:b", b"two").await.unwrap();
        kv.put("other", b"three").await.unwrap();

        assert_eq!(kv.get("prefix:a").await.unwrap(), Some(b"one".to_vec()));
        assert!(kv.exists("prefix:a").await.unwrap());
        assert!(!kv.exists("absent").await.unwrap());

        let page = kv
            .list(KvListOptions::new().with_prefix("prefix:"))
            .await
            .unwrap();
        assert_eq!(
            page.keys,
            vec!["prefix:a".to_owned(), "prefix:b".to_owned()]
        );
        assert!(page.cursor.is_none());

        assert_eq!(kv.list_all(None).await.unwrap().len(), 3);

        kv.delete("prefix:a").await.unwrap();
        assert_eq!(kv.get("prefix:a").await.unwrap(), None);
    }

    #[tokio::test]
    async fn list_all_drains_every_page_the_backend_hands_back() {
        let kv = Kv::new(InMemoryKeyValueStore::default());
        for key in ["a", "b", "c", "d", "e"] {
            kv.put(key, b"1").await.unwrap();
        }

        let first = kv.list(KvListOptions::new().with_limit(2)).await.unwrap();
        assert_eq!(first.keys, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(first.cursor.as_deref(), Some("b"));

        assert_eq!(
            kv.list_all(None).await.unwrap(),
            vec!["a", "b", "c", "d", "e"]
        );
    }

    #[tokio::test]
    async fn list_all_refuses_a_backend_that_repeats_its_cursor() {
        #[derive(Clone, Default)]
        struct StuckCursorStore;

        impl KeyValueStore for StuckCursorStore {
            async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, KvError> {
                Ok(None)
            }
            async fn put(&self, _key: &str, _value: &[u8]) -> Result<(), KvError> {
                Ok(())
            }
            async fn delete(&self, _key: &str) -> Result<(), KvError> {
                Ok(())
            }
            async fn list(&self, _options: KvListOptions) -> Result<KvListResult, KvError> {
                Ok(KvListResult {
                    keys: vec!["stuck".to_owned()],
                    cursor: Some("same".to_owned()),
                })
            }
        }

        let error = Kv::new(StuckCursorStore).list_all(None).await.unwrap_err();
        assert!(matches!(&error, KvError::Backend { message, .. } if message.contains("repeated")));
    }

    #[tokio::test]
    async fn atomic_primitives_default_to_unsupported() {
        let kv = Kv::new(InMemoryKeyValueStore::default());

        assert!(matches!(
            kv.put_if_absent("lock", b"held").await.unwrap_err(),
            KvError::Unsupported(_)
        ));
        assert!(matches!(
            kv.compare_and_swap("lock", None, b"held")
                .await
                .unwrap_err(),
            KvError::Unsupported(_)
        ));
        assert!(matches!(
            kv.increment("hits", 1).await.unwrap_err(),
            KvError::Unsupported(_)
        ));
        assert!(matches!(
            kv.expire("lock", core::time::Duration::from_secs(30))
                .await
                .unwrap_err(),
            KvError::Unsupported(_)
        ));

        // None of the refused operations may have written anything.
        assert_eq!(kv.list_all(None).await.unwrap(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn get_json_round_trips_and_reports_malformed_payloads() {
        let kv = Kv::new(InMemoryKeyValueStore::default());

        kv.put("good", br#"{"message":"hi"}"#).await.unwrap();
        let value: Greeting = kv.get_json("good").await.unwrap().unwrap();
        assert_eq!(value.message, "hi");

        assert!(kv.get_json::<Greeting>("absent").await.unwrap().is_none());

        kv.put("bad", b"not-json").await.unwrap();
        let error = kv.get_json::<Greeting>("bad").await.unwrap_err();
        assert!(matches!(error, KvError::Serialization(_)));
    }

    #[tokio::test]
    async fn get_text_reports_invalid_utf8_as_decode_error() {
        let kv = Kv::new(InMemoryKeyValueStore::default());

        kv.put("text", "héllo".as_bytes()).await.unwrap();
        assert_eq!(kv.get_text("text").await.unwrap().as_deref(), Some("héllo"));

        kv.put("binary", &[0xFF, 0xFE]).await.unwrap();
        let error = kv.get_text("binary").await.unwrap_err();
        assert!(matches!(error, KvError::Decode(_)));
    }

    #[tokio::test]
    async fn put_with_ttl_defaults_to_unsupported() {
        let kv = Kv::new(InMemoryKeyValueStore::default());

        let error = kv
            .put_with_ttl("key", b"value", core::time::Duration::from_mins(1))
            .await
            .unwrap_err();
        assert!(matches!(error, KvError::Unsupported(_)));

        // The failed put must not have stored anything.
        assert_eq!(kv.get("key").await.unwrap(), None);
    }

    #[tokio::test]
    async fn extractor_returns_internal_server_error_when_kv_is_missing() {
        let mut request = http_kit::Request::new(http_kit::Body::empty());

        let error = Kv::extract(&mut request).await.unwrap_err();

        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
