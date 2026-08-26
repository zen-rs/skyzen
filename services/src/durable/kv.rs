//! Durable Object key-value store abstraction.

use core::future::Future;

use serde::{de::DeserializeOwned, Serialize};

type DurableKvEntries = Vec<(String, Vec<u8>)>;

// ── Error type ──

/// Errors from Durable Object KV operations.
#[derive(Debug, thiserror::Error)]
pub enum DurableKvError {
    /// The underlying storage backend returned an error.
    #[error("durable kv error: {message}")]
    Backend {
        /// A human-readable description of what the backend was asked to do.
        message: String,
        /// The backend's own error, when it hands one back.
        #[source]
        source: Option<crate::BoxError>,
    },

    /// Serialization or deserialization failed.
    #[error("durable kv serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

backend_error!(DurableKvError);

service_http_error!(DurableKvError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Serialization(_) => INTERNAL_SERVER_ERROR,
});

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
    ) -> impl Future<Output = Result<Option<Vec<u8>>, DurableKvError>> + Send;

    /// Retrieve multiple values by keys.
    fn get_multiple(
        &self,
        keys: &[&str],
    ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + Send;

    /// Store a value under a key.
    fn put(
        &self,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<(), DurableKvError>> + Send;

    /// Store multiple key-value pairs atomically.
    fn put_multiple(
        &self,
        entries: &[(&str, &[u8])],
    ) -> impl Future<Output = Result<(), DurableKvError>> + Send;

    /// Delete a key. Returns whether the key existed.
    fn delete(&self, key: &str) -> impl Future<Output = Result<bool, DurableKvError>> + Send;

    /// Delete multiple keys. Returns the number of keys deleted.
    fn delete_multiple(
        &self,
        keys: &[&str],
    ) -> impl Future<Output = Result<usize, DurableKvError>> + Send;

    /// Delete all keys.
    fn delete_all(&self) -> impl Future<Output = Result<(), DurableKvError>> + Send;

    /// List key-value pairs matching the given options.
    fn list(
        &self,
        options: DurableListOptions<'_>,
    ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + Send;
}

// ── Layer 2: Generated object-safe trait ──

service_obj! {
    DurableKvStoreObj: DurableKvStore;
    async fn get<'a>(&'a self, key: &'a str) -> Result<Option<Vec<u8>>, DurableKvError>;
    async fn get_multiple<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> Result<DurableKvEntries, DurableKvError>;
    async fn put<'a>(&'a self, key: &'a str, value: &'a [u8]) -> Result<(), DurableKvError>;
    async fn put_multiple<'a>(
        &'a self,
        entries: &'a [(&'a str, &'a [u8])],
    ) -> Result<(), DurableKvError>;
    async fn delete<'a>(&'a self, key: &'a str) -> Result<bool, DurableKvError>;
    async fn delete_multiple<'a>(
        &'a self,
        keys: &'a [&'a str],
    ) -> Result<usize, DurableKvError>;
    async fn delete_all(&'_ self) -> Result<(), DurableKvError>;
    async fn list<'a>(
        &'a self,
        options: DurableListOptions<'a>,
    ) -> Result<DurableKvEntries, DurableKvError>;
}

// ── User-facing wrapper ──

/// Type-erased Durable Object KV extractor.
///
/// Wraps any [`DurableKvStore`] behind dynamic dispatch and provides
/// convenience methods for JSON operations.
pub struct DurableKv(Box<dyn DurableKvStoreObj>);

service_extractor!(
    DurableKv,
    DurableKvNotConfigured,
    "Durable KV store not configured. Ensure a DurableKvStore implementation is injected."
);

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

#[cfg(test)]
mod tests {
    use super::{
        DurableKv, DurableKvError, DurableKvNotConfigured, DurableKvStore, DurableListOptions,
    };
    use core::future::{ready, Future};
    use http_kit::{Body, Endpoint, HttpError, Response};
    use serde::{Deserialize, Serialize};
    use skyzen_core::Extractor;
    use std::{
        collections::BTreeMap,
        convert::Infallible,
        sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
    };

    #[derive(Clone, Default)]
    struct InMemoryDurableKvStore {
        data: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
    }

    impl InMemoryDurableKvStore {
        fn read(&self) -> Result<RwLockReadGuard<'_, BTreeMap<String, Vec<u8>>>, DurableKvError> {
            self.data
                .read()
                .map_err(|_| DurableKvError::backend("lock poisoned"))
        }

        fn write(&self) -> Result<RwLockWriteGuard<'_, BTreeMap<String, Vec<u8>>>, DurableKvError> {
            self.data
                .write()
                .map_err(|_| DurableKvError::backend("lock poisoned"))
        }

        fn list_entries(
            &self,
            options: &DurableListOptions<'_>,
        ) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
            let data = self.read()?;
            let iter: Box<dyn Iterator<Item = (&String, &Vec<u8>)>> = if options.reverse {
                Box::new(data.iter().rev())
            } else {
                Box::new(data.iter())
            };

            Ok(iter
                .filter(|(key, _)| {
                    options.prefix.is_none_or(|prefix| key.starts_with(prefix))
                        && options.start.is_none_or(|start| key.as_str() > start)
                        && options.end.is_none_or(|end| key.as_str() < end)
                })
                .take(options.limit.unwrap_or(usize::MAX))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect())
        }
    }

    // A `BTreeMap` behind a lock answers every call synchronously, so the futures are ready on
    // creation rather than `async` blocks with nothing to await.
    impl DurableKvStore for InMemoryDurableKvStore {
        fn get(
            &self,
            key: &str,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, DurableKvError>> + Send {
            ready(self.read().map(|data| data.get(key).cloned()))
        }

        fn get_multiple(
            &self,
            keys: &[&str],
        ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + Send {
            ready(self.read().map(|data| {
                keys.iter()
                    .filter_map(|key| {
                        data.get(*key)
                            .map(|value| ((*key).to_owned(), value.clone()))
                    })
                    .collect()
            }))
        }

        fn put(
            &self,
            key: &str,
            value: &[u8],
        ) -> impl Future<Output = Result<(), DurableKvError>> + Send {
            ready(self.write().map(|mut data| {
                data.insert(key.to_owned(), value.to_vec());
            }))
        }

        fn put_multiple(
            &self,
            entries: &[(&str, &[u8])],
        ) -> impl Future<Output = Result<(), DurableKvError>> + Send {
            ready(self.write().map(|mut data| {
                for (key, value) in entries {
                    data.insert((*key).to_owned(), value.to_vec());
                }
            }))
        }

        fn delete(&self, key: &str) -> impl Future<Output = Result<bool, DurableKvError>> + Send {
            ready(self.write().map(|mut data| data.remove(key).is_some()))
        }

        fn delete_multiple(
            &self,
            keys: &[&str],
        ) -> impl Future<Output = Result<usize, DurableKvError>> + Send {
            ready(self.write().map(|mut data| {
                keys.iter()
                    .filter(|key| data.remove(**key).is_some())
                    .count()
            }))
        }

        fn delete_all(&self) -> impl Future<Output = Result<(), DurableKvError>> + Send {
            ready(self.write().map(|mut data| data.clear()))
        }

        fn list(
            &self,
            options: DurableListOptions<'_>,
        ) -> impl Future<Output = Result<Vec<(String, Vec<u8>)>, DurableKvError>> + Send {
            ready(self.list_entries(&options))
        }
    }

    #[derive(Debug, Clone)]
    struct ReadDurableKvEndpoint;

    impl Endpoint for ReadDurableKvEndpoint {
        type Error = Infallible;

        async fn respond(
            &mut self,
            request: &mut http_kit::Request,
        ) -> Result<Response, Self::Error> {
            let kv = DurableKv::extract(request)
                .await
                .expect("durable kv should be injected");
            let body = kv
                .get("message")
                .await
                .expect("durable kv access should succeed")
                .expect("message should exist");
            Ok(Response::new(Body::from(body)))
        }
    }

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct Session {
        user: String,
    }

    #[tokio::test]
    async fn wrapper_supports_crud_listing_and_json_helpers() {
        let kv = DurableKv::new(InMemoryDurableKvStore::default());

        kv.put("alpha", b"1").await.unwrap();
        kv.put_multiple(&[("beta", b"2"), ("prefix:1", b"3"), ("prefix:2", b"4")])
            .await
            .unwrap();
        kv.put_json(
            "session",
            &Session {
                user: "lexo".to_owned(),
            },
        )
        .await
        .unwrap();

        assert_eq!(kv.get("alpha").await.unwrap(), Some(b"1".to_vec()));
        assert_eq!(
            kv.get_multiple(&["alpha", "missing", "beta"])
                .await
                .unwrap(),
            vec![
                ("alpha".to_owned(), b"1".to_vec()),
                ("beta".to_owned(), b"2".to_vec()),
            ]
        );
        assert_eq!(
            kv.get_json::<Session>("session").await.unwrap(),
            Some(Session {
                user: "lexo".to_owned(),
            })
        );

        let listed = kv
            .list(DurableListOptions {
                prefix: Some("prefix:"),
                start: Some("prefix:1"),
                end: None,
                limit: Some(1),
                reverse: false,
            })
            .await
            .unwrap();
        assert_eq!(listed, vec![("prefix:2".to_owned(), b"4".to_vec())]);

        let reverse_list = kv
            .list(DurableListOptions {
                prefix: Some("prefix:"),
                start: None,
                end: None,
                limit: None,
                reverse: true,
            })
            .await
            .unwrap();
        assert_eq!(reverse_list[0].0, "prefix:2");

        assert!(kv.delete("alpha").await.unwrap());
        assert_eq!(kv.delete_multiple(&["beta", "missing"]).await.unwrap(), 1);
        kv.delete_all().await.unwrap();
        assert!(kv
            .list(DurableListOptions::default())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn get_json_surfaces_deserialization_errors() {
        let kv = DurableKv::new(InMemoryDurableKvStore::default());
        kv.put("session", b"not-json").await.unwrap();

        let error = kv.get_json::<Session>("session").await.unwrap_err();

        assert!(matches!(error, DurableKvError::Serialization(_)));
    }

    #[tokio::test]
    async fn middleware_injects_durable_kv_for_downstream_endpoint_and_extractor() {
        let store = InMemoryDurableKvStore::default();
        store.put("message", b"hello").await.unwrap();
        let kv = DurableKv::new(store);
        let mut request = http_kit::Request::new(Body::empty());

        let response = ::skyzen_core::middleware::apply(&kv, &mut request, ReadDurableKvEndpoint)
            .await
            .unwrap();
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "hello");

        let extracted = DurableKv::extract(&mut request).await.unwrap();
        assert_eq!(
            extracted.get("message").await.unwrap(),
            Some(b"hello".to_vec())
        );
    }

    #[tokio::test]
    async fn extractor_returns_internal_server_error_when_durable_kv_is_missing() {
        let mut request = http_kit::Request::new(Body::empty());

        let error = DurableKv::extract(&mut request).await.unwrap_err();

        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn missing_configuration_error_uses_expected_status() {
        let error = DurableKvNotConfigured::new();
        assert_eq!(
            error.status(),
            skyzen_core::StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
