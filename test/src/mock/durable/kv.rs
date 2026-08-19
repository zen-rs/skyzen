//! In-memory Durable Object KV store for testing.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use skyzen_services::durable::kv::{DurableKvError, DurableKvStore, DurableListOptions};

/// In-memory implementation of [`DurableKvStore`] for testing.
#[derive(Debug, Clone, Default)]
pub struct InMemoryDurableKv {
    data: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
}

impl InMemoryDurableKv {
    /// Create a new empty in-memory Durable KV store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl DurableKvStore for InMemoryDurableKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, DurableKvError> {
        let data = self.data.read().map_err(lock_err)?;
        Ok(data.get(key).cloned())
    }

    async fn get_multiple(&self, keys: &[&str]) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        let data = self.data.read().map_err(lock_err)?;
        Ok(keys
            .iter()
            .filter_map(|k| data.get(*k).map(|v| ((*k).to_owned(), v.clone())))
            .collect())
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), DurableKvError> {
        self.data
            .write()
            .map_err(lock_err)?
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    async fn put_multiple(&self, entries: &[(&str, &[u8])]) -> Result<(), DurableKvError> {
        let mut guard = self.data.write().map_err(lock_err)?;
        for (k, v) in entries {
            guard.insert((*k).to_owned(), v.to_vec());
        }
        drop(guard);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<bool, DurableKvError> {
        Ok(self.data.write().map_err(lock_err)?.remove(key).is_some())
    }

    async fn delete_multiple(&self, keys: &[&str]) -> Result<usize, DurableKvError> {
        let mut guard = self.data.write().map_err(lock_err)?;
        let count = keys.iter().filter(|k| guard.remove(**k).is_some()).count();
        drop(guard);
        Ok(count)
    }

    async fn delete_all(&self) -> Result<(), DurableKvError> {
        self.data.write().map_err(lock_err)?.clear();
        Ok(())
    }

    async fn list(
        &self,
        options: DurableListOptions<'_>,
    ) -> Result<Vec<(String, Vec<u8>)>, DurableKvError> {
        let data = self.data.read().map_err(lock_err)?;
        let iter: Box<dyn Iterator<Item = (&String, &Vec<u8>)>> = if options.reverse {
            Box::new(data.iter().rev())
        } else {
            Box::new(data.iter())
        };

        let result: Vec<(String, Vec<u8>)> = iter
            .filter(|(k, _)| {
                if let Some(prefix) = options.prefix {
                    if !k.starts_with(prefix) {
                        return false;
                    }
                }
                if let Some(start) = options.start {
                    if k.as_str() <= start {
                        return false;
                    }
                }
                if let Some(end) = options.end {
                    if k.as_str() >= end {
                        return false;
                    }
                }
                true
            })
            .take(options.limit.unwrap_or(usize::MAX))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        Ok(result)
    }
}

fn lock_err<T>(_: T) -> DurableKvError {
    DurableKvError::Backend("lock poisoned".to_owned())
}

#[cfg(test)]
mod tests {
    use skyzen_services::durable::kv::{DurableKvStore, DurableListOptions};

    use super::InMemoryDurableKv;

    async fn seeded() -> InMemoryDurableKv {
        let kv = InMemoryDurableKv::new();
        for key in ["a", "b", "c", "d"] {
            kv.put(key, key.as_bytes()).await.unwrap();
        }
        kv
    }

    fn keys(entries: Vec<(String, Vec<u8>)>) -> Vec<String> {
        entries.into_iter().map(|(key, _)| key).collect()
    }

    /// Pins the documented contract: `start` is **exclusive**.
    #[tokio::test]
    async fn list_start_bound_is_exclusive() {
        let kv = seeded().await;
        let entries = kv
            .list(DurableListOptions {
                start: Some("b"),
                ..DurableListOptions::default()
            })
            .await
            .unwrap();
        assert_eq!(keys(entries), vec!["c", "d"]);
    }

    /// Pins the documented contract: `end` is **exclusive**.
    #[tokio::test]
    async fn list_end_bound_is_exclusive() {
        let kv = seeded().await;
        let entries = kv
            .list(DurableListOptions {
                end: Some("c"),
                ..DurableListOptions::default()
            })
            .await
            .unwrap();
        assert_eq!(keys(entries), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn list_reverse_returns_descending_order() {
        let kv = seeded().await;
        let entries = kv
            .list(DurableListOptions {
                reverse: true,
                ..DurableListOptions::default()
            })
            .await
            .unwrap();
        assert_eq!(keys(entries), vec!["d", "c", "b", "a"]);
    }

    /// `limit` applies after ordering, so a reverse listing keeps the
    /// **highest** keys.
    #[tokio::test]
    async fn list_limit_truncates_after_ordering() {
        let kv = seeded().await;

        let ascending = kv
            .list(DurableListOptions {
                limit: Some(2),
                ..DurableListOptions::default()
            })
            .await
            .unwrap();
        assert_eq!(keys(ascending), vec!["a", "b"]);

        let descending = kv
            .list(DurableListOptions {
                limit: Some(2),
                reverse: true,
                ..DurableListOptions::default()
            })
            .await
            .unwrap();
        assert_eq!(keys(descending), vec!["d", "c"]);
    }

    #[tokio::test]
    async fn list_combines_prefix_bounds_and_reverse() {
        let kv = InMemoryDurableKv::new();
        for key in ["p:1", "p:2", "p:3", "q:1"] {
            kv.put(key, b"v").await.unwrap();
        }

        let entries = kv
            .list(DurableListOptions {
                prefix: Some("p:"),
                start: Some("p:1"),
                end: Some("p:3"),
                reverse: true,
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(keys(entries), vec!["p:2"]);
    }
}
