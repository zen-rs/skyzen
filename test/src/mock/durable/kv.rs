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
