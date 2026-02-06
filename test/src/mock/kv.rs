//! In-memory key-value store for testing.

use std::{collections::HashMap, sync::Arc, sync::RwLock};

use skyzen_services::kv::{KeyValueStore, KvError};

/// An in-memory key-value store backed by a `HashMap`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
#[derive(Debug, Clone)]
pub struct InMemoryKv {
    data: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl InMemoryKv {
    /// Create a new empty in-memory KV store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryKv {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyValueStore for InMemoryKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let data = self.data.read().expect("InMemoryKv lock poisoned");
        Ok(data.get(key).cloned())
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.data
            .write()
            .expect("InMemoryKv lock poisoned")
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.data
            .write()
            .expect("InMemoryKv lock poisoned")
            .remove(key);
        Ok(())
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let data = self.data.read().expect("InMemoryKv lock poisoned");
        let keys: Vec<String> = prefix.map_or_else(
            || data.keys().cloned().collect(),
            |p| data.keys().filter(|k| k.starts_with(p)).cloned().collect(),
        );
        drop(data);
        Ok(keys)
    }
}
