//! In-memory key-value store for testing.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use skyzen_services::kv::{KeyValueStore, KvError};

#[derive(Debug, Clone)]
struct Entry {
    value: Vec<u8>,
    /// The instant at which the entry stops being visible; `None` never expires.
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|expires_at| now >= expires_at)
    }
}

/// An in-memory key-value store backed by a `HashMap`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
///
/// Supports TTL via [`KeyValueStore::put_with_ttl`]: expired keys are absent
/// from `get` and `list` and are physically removed on the next mutation or
/// listing. [`list`](KeyValueStore::list) returns keys in lexicographic order.
///
/// Use [`fail_next_with`](Self::fail_next_with) to make the next operation
/// fail, e.g. to exercise handler error paths.
#[derive(Debug, Clone)]
pub struct InMemoryKv {
    data: Arc<RwLock<HashMap<String, Entry>>>,
    fail_next: Arc<RwLock<Option<String>>>,
}

impl InMemoryKv {
    /// Create a new empty in-memory KV store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
            fail_next: Arc::new(RwLock::new(None)),
        }
    }

    /// Make exactly the next store operation fail with
    /// [`KvError::Backend`] carrying `message`.
    ///
    /// Subsequent operations succeed again. Useful for testing how handlers
    /// react to backend failures (typically a 500 response).
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn fail_next_with(&self, message: &str) {
        *self.fail_next.write().expect("InMemoryKv lock poisoned") = Some(message.to_owned());
    }

    fn take_injected_failure(&self) -> Result<(), KvError> {
        let mut slot = self.fail_next.write().expect("InMemoryKv lock poisoned");
        slot.take().map_or(Ok(()), |message| {
            drop(slot);
            Err(KvError::backend(message))
        })
    }

    /// Remove expired entries so they are eventually physically cleaned up.
    fn purge_expired(&self) {
        let now = Instant::now();
        self.data
            .write()
            .expect("InMemoryKv lock poisoned")
            .retain(|_, entry| !entry.is_expired(now));
    }

    fn insert(&self, key: &str, value: &[u8], expires_at: Option<Instant>) {
        self.data.write().expect("InMemoryKv lock poisoned").insert(
            key.to_owned(),
            Entry {
                value: value.to_vec(),
                expires_at,
            },
        );
    }
}

impl Default for InMemoryKv {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyValueStore for InMemoryKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        self.take_injected_failure()?;
        let now = Instant::now();
        let data = self.data.read().expect("InMemoryKv lock poisoned");
        Ok(data
            .get(key)
            .filter(|entry| !entry.is_expired(now))
            .map(|entry| entry.value.clone()))
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.take_injected_failure()?;
        self.purge_expired();
        self.insert(key, value, None);
        Ok(())
    }

    async fn put_with_ttl(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), KvError> {
        self.take_injected_failure()?;
        self.purge_expired();
        // A TTL too large to represent never expires.
        let expires_at = Instant::now().checked_add(ttl);
        self.insert(key, value, expires_at);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.take_injected_failure()?;
        self.data
            .write()
            .expect("InMemoryKv lock poisoned")
            .remove(key);
        Ok(())
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        self.take_injected_failure()?;
        self.purge_expired();
        let data = self.data.read().expect("InMemoryKv lock poisoned");
        let mut keys: Vec<String> = prefix.map_or_else(
            || data.keys().cloned().collect(),
            |p| data.keys().filter(|k| k.starts_with(p)).cloned().collect(),
        );
        drop(data);
        keys.sort_unstable();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use skyzen_services::kv::{KeyValueStore, KvError};

    use super::InMemoryKv;

    #[tokio::test]
    async fn list_returns_keys_in_lexicographic_order() {
        let kv = InMemoryKv::new();
        for key in ["b", "c", "a", "aa"] {
            kv.put(key, b"1").await.unwrap();
        }

        assert_eq!(kv.list(None).await.unwrap(), vec!["a", "aa", "b", "c"]);
        assert_eq!(kv.list(Some("a")).await.unwrap(), vec!["a", "aa"]);
    }

    #[tokio::test]
    async fn zero_ttl_expires_immediately_for_get_and_list() {
        let kv = InMemoryKv::new();
        kv.put("keep", b"forever").await.unwrap();
        kv.put_with_ttl("gone", b"soon", Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(kv.get("gone").await.unwrap(), None);
        assert_eq!(kv.list(None).await.unwrap(), vec!["keep"]);
        // The expired entry is physically removed by the list purge.
        assert_eq!(kv.data.read().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn positive_ttl_keeps_the_value_visible_until_expiry() {
        let kv = InMemoryKv::new();
        kv.put_with_ttl("session", b"data", Duration::from_hours(1))
            .await
            .unwrap();

        assert_eq!(kv.get("session").await.unwrap(), Some(b"data".to_vec()));
        assert_eq!(kv.list(None).await.unwrap(), vec!["session"]);
    }

    #[tokio::test]
    async fn overwriting_with_plain_put_clears_the_ttl() {
        let kv = InMemoryKv::new();
        kv.put_with_ttl("key", b"ttl", Duration::ZERO)
            .await
            .unwrap();
        kv.put("key", b"forever").await.unwrap();

        assert_eq!(kv.get("key").await.unwrap(), Some(b"forever".to_vec()));
    }

    #[tokio::test]
    async fn fail_next_with_fails_exactly_one_operation() {
        let kv = InMemoryKv::new();
        kv.put("key", b"value").await.unwrap();
        kv.fail_next_with("redis is down");

        let error = kv.get("key").await.unwrap_err();
        assert!(matches!(&error, KvError::Backend { message, .. } if message == "redis is down"));

        // The failure is consumed; the store works again.
        assert_eq!(kv.get("key").await.unwrap(), Some(b"value".to_vec()));
    }
}
