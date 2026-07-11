//! In-memory object storage for testing.

use std::{collections::HashMap, sync::Arc, sync::RwLock};

use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, StorageError, StorageObject,
};

/// An in-memory object storage backed by a `HashMap`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
#[derive(Debug, Clone)]
pub struct InMemoryStorage {
    data: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl InMemoryStorage {
    /// Create a new empty in-memory storage.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

fn metadata_for(key: &str, body: &[u8]) -> ObjectMetadata {
    ObjectMetadata {
        key: key.to_owned(),
        size: body.len() as u64,
        content_type: None,
        last_modified: None,
        metadata: HashMap::new(),
    }
}

impl ObjectStorage for InMemoryStorage {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let data = self.data.read().expect("InMemoryStorage lock poisoned");
        Ok(data.get(key).map(|body| StorageObject {
            metadata: metadata_for(key, body),
            body: body.clone(),
        }))
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.data
            .write()
            .expect("InMemoryStorage lock poisoned")
            .insert(key.to_owned(), body);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.data
            .write()
            .expect("InMemoryStorage lock poisoned")
            .remove(key);
        Ok(())
    }

    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        let data = self.data.read().expect("InMemoryStorage lock poisoned");
        let mut objects: Vec<ObjectMetadata> = data
            .iter()
            .filter(|(k, _)| {
                options
                    .prefix
                    .as_ref()
                    .is_none_or(|prefix| k.starts_with(prefix))
            })
            .map(|(k, v)| metadata_for(k, v))
            .collect();
        drop(data);

        objects.sort_by(|a, b| a.key.cmp(&b.key));

        if let Some(limit) = options.limit {
            objects.truncate(limit);
        }

        Ok(ListResult {
            objects,
            cursor: None,
        })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        let data = self.data.read().expect("InMemoryStorage lock poisoned");
        Ok(data.get(key).map(|body| metadata_for(key, body)))
    }
}
