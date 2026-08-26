//! In-memory object storage for testing.

use core::future::{ready, Future};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http_kit::{
    header::{self, HeaderValue},
    Method,
};

use skyzen_services::storage::{
    ByteRange, ListOptions, ListResult, ObjectMetadata, ObjectStorage, PresignedRequest,
    PutOptions, StorageError, StorageObject, StorageStream,
};

/// The scheme of the URLs [`InMemoryStorage`] presigns.
///
/// Deliberately not `http`: nothing serves it, so a test that accidentally fetches a presigned URL
/// fails at connect time instead of silently hitting a real bucket.
const MEMORY_URL_SCHEME: &str = "memory";

#[derive(Debug, Clone)]
struct StoredObject {
    body: Vec<u8>,
    /// Everything the put asked to record, kept whole so no option is dropped as
    /// [`PutOptions`] grows.
    options: PutOptions,
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
/// The byte-path additions are implemented over the same buffers:
/// [`get_stream`](ObjectStorage::get_stream) yields the object as one chunk,
/// [`put_stream`](ObjectStorage::put_stream) drains the stream and rejects a `content_length` that
/// does not match what arrived, and [`get_range`](ObjectStorage::get_range) does real range
/// arithmetic while still reporting the whole object's size in the metadata.
/// [`presign_get`](ObjectStorage::presign_get) and
/// [`presign_put`](ObjectStorage::presign_put) return a deterministic `memory://` URL that is
/// **not fetchable** — nothing serves that scheme, so a test that accidentally follows one fails
/// instead of reaching a real bucket.
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
                    options,
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
        content_type: object.options.content_type.clone(),
        last_modified: Some(object.last_modified),
        metadata: object.options.metadata.clone(),
        // A HashMap keeps no version history and mints no entity tag, and inventing either would
        // let a test assert on something no real backend guarantees.
        etag: None,
        version: None,
    }
}

// A `HashMap` behind a lock answers every call but `put_stream` synchronously, so those futures
// are ready on creation rather than `async` blocks with nothing to await.
impl ObjectStorage for InMemoryStorage {
    fn get(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<StorageObject>, StorageError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let data = self.data.read().expect("InMemoryStorage lock poisoned");
            data.get(key).map(|object| StorageObject {
                metadata: metadata_for(key, object),
                body: object.body.clone(),
            })
        }))
    }

    fn put(
        &self,
        key: &str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        ready(
            self.take_injected_failure()
                .map(|()| self.insert(key, body, PutOptions::default())),
        )
    }

    fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> impl Future<Output = Result<(), StorageError>> + Send {
        ready(
            self.take_injected_failure()
                .map(|()| self.insert(key, body, options)),
        )
    }

    fn delete(&self, key: &str) -> impl Future<Output = Result<(), StorageError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            self.data
                .write()
                .expect("InMemoryStorage lock poisoned")
                .remove(key);
        }))
    }

    fn list(
        &self,
        options: ListOptions,
    ) -> impl Future<Output = Result<ListResult, StorageError>> + Send {
        ready(self.take_injected_failure().map(|()| {
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

            ListResult { objects, cursor }
        }))
    }

    fn head(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<ObjectMetadata>, StorageError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let data = self.data.read().expect("InMemoryStorage lock poisoned");
            data.get(key).map(|object| metadata_for(key, object))
        }))
    }

    /// Stream the stored buffer back as a single chunk.
    ///
    /// The mock has nothing to chunk over, so the stream yields once; what it exercises is the
    /// caller's streaming path, not the backend's.
    fn get_stream(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<StorageStream>, StorageError>> + Send {
        ready(self.take_injected_failure().map(|()| {
            let data = self.data.read().expect("InMemoryStorage lock poisoned");
            data.get(key)
                .map(|object| StorageStream::once(object.body.clone()))
        }))
    }

    /// Drain the stream into a buffer and store it.
    ///
    /// `content_length` is checked against what actually arrived, so a test that lies about the
    /// size fails here rather than in whichever backend would have rejected it.
    async fn put_stream(
        &self,
        key: &str,
        stream: StorageStream,
        content_length: Option<u64>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        self.take_injected_failure()?;
        let body = stream.into_bytes().await?;
        if let Some(expected) = content_length {
            let actual = body.len() as u64;
            if actual != expected {
                return Err(StorageError::Io(format!(
                    "declared a content length of {expected} bytes but the stream carried {actual}"
                )));
            }
        }
        self.insert(key, body, options);
        Ok(())
    }

    fn get_range(
        &self,
        key: &str,
        range: ByteRange,
    ) -> impl Future<Output = Result<Option<StorageObject>, StorageError>> + Send {
        ready(self.take_injected_failure().and_then(|()| {
            let stored = {
                let data = self.data.read().expect("InMemoryStorage lock poisoned");
                data.get(key).cloned()
            };
            let Some(object) = stored else {
                return Ok(None);
            };

            let total = object.body.len() as u64;
            let resolved = range.resolve(total).ok_or_else(|| {
                StorageError::Io(format!(
                    "range {range:?} selects nothing in an object of {total} bytes"
                ))
            })?;
            // `resolve` clamps both ends to the buffer's own length, so neither conversion can
            // fail.
            let bounds = usize::try_from(resolved.start)
                .and_then(|start| usize::try_from(resolved.end).map(|end| start..end));
            let bounds = bounds.map_err(|_| {
                StorageError::Io(format!(
                    "range {range:?} does not fit this platform's usize"
                ))
            })?;

            // The metadata keeps reporting the whole object's size so a caller can build a
            // `Content-Range` from the pair.
            Ok(Some(StorageObject {
                body: object.body[bounds].to_vec(),
                metadata: metadata_for(key, &object),
            }))
        }))
    }

    fn presign_get(
        &self,
        key: &str,
        expires_in: Duration,
    ) -> impl Future<Output = Result<PresignedRequest, StorageError>> + Send {
        ready(
            self.take_injected_failure()
                .map(|()| presigned(Method::GET, key, expires_in)),
        )
    }

    fn presign_put(
        &self,
        key: &str,
        expires_in: Duration,
        options: PutOptions,
    ) -> impl Future<Output = Result<PresignedRequest, StorageError>> + Send {
        ready(self.take_injected_failure().and_then(|()| {
            // Only the content type turns into a header the presigned request can carry; anything
            // further would have to be dropped, so it is refused instead.
            options.reject_unsupported(&[])?;
            let mut request = presigned(Method::PUT, key, expires_in);
            if let Some(content_type) = options.content_type {
                let value = HeaderValue::from_str(&content_type).map_err(|error| {
                    StorageError::Io(format!(
                        "content type {content_type:?} is not a header value: {error}"
                    ))
                })?;
                request.headers.push((header::CONTENT_TYPE, value));
            }
            Ok(request)
        }))
    }
}

/// Build the deterministic stand-in URL the mock presigns.
///
/// The value is stable for a given key and expiry so snapshot tests can assert on it, and it is
/// **not fetchable**: nothing serves the `memory:` scheme.
fn presigned(method: Method, key: &str, expires_in: Duration) -> PresignedRequest {
    PresignedRequest {
        url: format!(
            "{MEMORY_URL_SCHEME}://in-memory-storage/{key}?expires_in={}",
            expires_in.as_secs()
        ),
        method,
        headers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, time::Duration};

    use http_kit::Method;
    use skyzen_services::storage::{
        ByteRange, ListOptions, ObjectStorage, PutOptions, StorageError, StorageStream,
    };

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
                metadata.iter().fold(
                    PutOptions::new().with_content_type("application/pdf"),
                    |options, (name, value)| options.with_metadata(name, value),
                ),
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
    async fn get_stream_and_put_stream_round_trip_a_body() {
        let storage = InMemoryStorage::new();
        storage
            .put_stream(
                "clip.bin",
                StorageStream::once(b"streamed".to_vec()),
                Some(8),
                PutOptions::new().with_content_type("application/octet-stream"),
            )
            .await
            .unwrap();

        let stream = storage.get_stream("clip.bin").await.unwrap().unwrap();
        assert_eq!(stream.into_bytes().await.unwrap(), b"streamed".to_vec());

        let head = storage.head("clip.bin").await.unwrap().unwrap();
        assert_eq!(
            head.content_type.as_deref(),
            Some("application/octet-stream")
        );

        assert!(storage.get_stream("absent.bin").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_stream_rejects_a_content_length_that_does_not_match() {
        let storage = InMemoryStorage::new();

        let error = storage
            .put_stream(
                "clip.bin",
                StorageStream::once(b"four".to_vec()),
                Some(9000),
                PutOptions::default(),
            )
            .await
            .unwrap_err();

        assert!(matches!(&error, StorageError::Io(message) if message.contains("9000")));
        assert!(storage.head("clip.bin").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_range_slices_the_body_and_keeps_the_full_size_in_the_metadata() {
        let storage = InMemoryStorage::new();
        storage
            .put("alphabet.txt", b"abcdefghij".to_vec())
            .await
            .unwrap();

        let middle = storage
            .get_range("alphabet.txt", ByteRange::slice(2, 3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(middle.body, b"cde".to_vec());
        assert_eq!(middle.metadata.size, 10);

        let tail = storage
            .get_range("alphabet.txt", ByteRange::suffix(3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tail.body, b"hij".to_vec());

        let rest = storage
            .get_range("alphabet.txt", ByteRange::from_start(7))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rest.body, b"hij".to_vec());

        assert!(storage
            .get_range("absent.txt", ByteRange::from_start(0))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn get_range_reports_an_unsatisfiable_range() {
        let storage = InMemoryStorage::new();
        storage.put("short.txt", b"ab".to_vec()).await.unwrap();

        let error = storage
            .get_range("short.txt", ByteRange::from_start(5))
            .await
            .unwrap_err();
        assert!(matches!(error, StorageError::Io(_)));
    }

    #[tokio::test]
    async fn presigning_returns_a_deterministic_url_that_is_not_fetchable() {
        let storage = InMemoryStorage::new();

        let get = storage
            .presign_get("report.pdf", Duration::from_mins(15))
            .await
            .unwrap();
        assert_eq!(get.method, Method::GET);
        assert_eq!(
            get.url,
            "memory://in-memory-storage/report.pdf?expires_in=900"
        );
        assert!(get.headers.is_empty());
        assert_eq!(
            storage
                .presign_get("report.pdf", Duration::from_mins(15))
                .await
                .unwrap()
                .url,
            get.url
        );

        let put = storage
            .presign_put(
                "report.pdf",
                Duration::from_mins(15),
                PutOptions::new().with_content_type("application/pdf"),
            )
            .await
            .unwrap();
        assert_eq!(put.method, Method::PUT);
        assert_eq!(
            put.headers,
            vec![(
                http_kit::header::CONTENT_TYPE,
                http_kit::header::HeaderValue::from_static("application/pdf")
            )]
        );
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
