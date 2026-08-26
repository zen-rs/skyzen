//! In-memory object storage for testing.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, PutOptions, StorageError, StorageObject,
};

#[derive(Debug, Clone)]
struct StoredObject {
    body: Vec<u8>,
    content_type: Option<String>,
    metadata: HashMap<String, String>,
    last_modified: u64,
}

/// An in-memory object storage backed by a `HashMap`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
///
/// Content type and custom metadata stored via
/// [`ObjectStorage::put_with`] are returned by `get`, `head`, and `list`,
/// and `list` implements real cursor pagination: when a page is truncated by
/// `limit`, the returned cursor is the last key of the page and the next
/// request resumes strictly after it.
///
/// Use [`fail_next_with`](Self::fail_next_with) to make the next operation
/// fail, e.g. to exercise handler error paths.
#[derive(Debug, Clone)]
pub struct InMemoryStorage {
    data: Arc<RwLock<HashMap<String, StoredObject>>>,
    fail_next: Arc<RwLock<Option<String>>>,
}

impl InMemoryStorage {
    /// Create a new empty in-memory storage.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
            fail_next: Arc::new(RwLock::new(None)),
        }
    }

    /// Make exactly the next storage operation fail with
    /// [`StorageError::Backend`] carrying `message`.
    ///
    /// Subsequent operations succeed again. Useful for testing how handlers
    /// react to backend failures (typically a 500 response).
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn fail_next_with(&self, message: &str) {
        *self
            .fail_next
            .write()
            .expect("InMemoryStorage lock poisoned") = Some(message.to_owned());
    }

    fn take_injected_failure(&self) -> Result<(), StorageError> {
        let mut slot = self
            .fail_next
            .write()
            .expect("InMemoryStorage lock poisoned");
        slot.take().map_or(Ok(()), |message| {
            drop(slot);
            Err(StorageError::backend(message))
        })
    }

    fn insert(&self, key: &str, body: Vec<u8>, options: PutOptions) {
        self.data
            .write()
            .expect("InMemoryStorage lock poisoned")
            .insert(
                key.to_owned(),
                StoredObject {
                    body,
                    content_type: options.content_type,
                    metadata: options.metadata,
                    last_modified: unix_now(),
                },
            );
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn metadata_for(key: &str, object: &StoredObject) -> ObjectMetadata {
    ObjectMetadata {
        key: key.to_owned(),
        size: object.body.len() as u64,
        content_type: object.content_type.clone(),
        last_modified: Some(object.last_modified),
        metadata: object.metadata.clone(),
    }
}

impl ObjectStorage for InMemoryStorage {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        self.take_injected_failure()?;
        let data = self.data.read().expect("InMemoryStorage lock poisoned");
        Ok(data.get(key).map(|object| StorageObject {
            metadata: metadata_for(key, object),
            body: object.body.clone(),
        }))
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.take_injected_failure()?;
        self.insert(key, body, PutOptions::default());
        Ok(())
    }

    async fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        self.take_injected_failure()?;
        self.insert(key, body, options);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.take_injected_failure()?;
        self.data
            .write()
            .expect("InMemoryStorage lock poisoned")
            .remove(key);
        Ok(())
    }

    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        self.take_injected_failure()?;
        let data = self.data.read().expect("InMemoryStorage lock poisoned");
        let mut objects: Vec<ObjectMetadata> = data
            .iter()
            .filter(|(key, _)| {
                options
                    .prefix
                    .as_ref()
                    .is_none_or(|prefix| key.starts_with(prefix))
            })
            .filter(|(key, _)| {
                // The cursor is the last key of the previous page; resume
                // strictly after it.
                options
                    .cursor
                    .as_ref()
                    .is_none_or(|cursor| key.as_str() > cursor.as_str())
            })
            .map(|(key, object)| metadata_for(key, object))
            .collect();
        drop(data);

        objects.sort_by(|a, b| a.key.cmp(&b.key));

        let mut cursor = None;
        if let Some(limit) = options.limit {
            if objects.len() > limit {
                objects.truncate(limit);
                cursor = objects.last().map(|object| object.key.clone());
            }
        }

        Ok(ListResult { objects, cursor })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        self.take_injected_failure()?;
        let data = self.data.read().expect("InMemoryStorage lock poisoned");
        Ok(data.get(key).map(|object| metadata_for(key, object)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use skyzen_services::storage::{ListOptions, ObjectStorage, PutOptions, StorageError};

    use super::InMemoryStorage;

    #[tokio::test]
    async fn list_paginates_with_cursor_across_three_pages() {
        let storage = InMemoryStorage::new();
        for key in ["a", "b", "c", "d", "e"] {
            storage.put(key, key.as_bytes().to_vec()).await.unwrap();
        }

        let mut pages = Vec::new();
        let mut cursor = None;
        loop {
            let result = storage
                .list(ListOptions {
                    prefix: None,
                    limit: Some(2),
                    cursor: cursor.take(),
                })
                .await
                .unwrap();
            pages.push(
                result
                    .objects
                    .iter()
                    .map(|object| object.key.clone())
                    .collect::<Vec<_>>(),
            );
            match result.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(
            pages,
            vec![
                vec!["a".to_owned(), "b".to_owned()],
                vec!["c".to_owned(), "d".to_owned()],
                vec!["e".to_owned()],
            ]
        );
    }

    #[tokio::test]
    async fn list_returns_no_cursor_when_the_page_is_complete() {
        let storage = InMemoryStorage::new();
        storage.put("a", b"1".to_vec()).await.unwrap();
        storage.put("b", b"2".to_vec()).await.unwrap();

        let exact = storage
            .list(ListOptions {
                prefix: None,
                limit: Some(2),
                cursor: None,
            })
            .await
            .unwrap();
        assert_eq!(exact.objects.len(), 2);
        assert!(exact.cursor.is_none());
    }

    #[tokio::test]
    async fn put_with_round_trips_content_type_and_metadata() {
        let storage = InMemoryStorage::new();
        let mut metadata = HashMap::new();
        metadata.insert("owner".to_owned(), "tests".to_owned());
        storage
            .put_with(
                "report.pdf",
                b"%PDF".to_vec(),
                PutOptions {
                    content_type: Some("application/pdf".to_owned()),
                    metadata: metadata.clone(),
                },
            )
            .await
            .unwrap();

        let object = storage.get("report.pdf").await.unwrap().unwrap();
        assert_eq!(
            object.metadata.content_type.as_deref(),
            Some("application/pdf")
        );
        assert_eq!(object.metadata.metadata, metadata);
        assert!(object.metadata.last_modified.is_some());

        let head = storage.head("report.pdf").await.unwrap().unwrap();
        assert_eq!(head.content_type.as_deref(), Some("application/pdf"));
        assert_eq!(head.metadata, metadata);
    }

    #[tokio::test]
    async fn fail_next_with_fails_exactly_one_operation() {
        let storage = InMemoryStorage::new();
        storage.put("key", b"value".to_vec()).await.unwrap();
        storage.fail_next_with("bucket unavailable");

        let error = storage.get("key").await.unwrap_err();
        assert!(
            matches!(&error, StorageError::Backend { message, .. } if message == "bucket unavailable")
        );

        assert!(storage.get("key").await.unwrap().is_some());
    }
}
