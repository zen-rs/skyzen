//! Azure Blob Storage implementation of [`ObjectStorage`].

use std::collections::HashMap;

use base64::Engine;
use futures_util::TryStreamExt;
pub use opendal::services::Azblob;
use opendal::{Entry, ErrorKind, Operator};
use serde::{Deserialize, Serialize};
use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, PutOptions, StorageError, StorageObject,
};

/// An Azure Blob Storage-backed object store.
///
/// Configure an [`Azblob`] builder, then pass it to [`AzureBlob::new`].
/// The resulting `OpenDAL` operator is cheap to clone and owns the Azure client.
#[derive(Clone, Debug)]
pub struct AzureBlob {
    operator: Operator,
}

impl AzureBlob {
    /// Create an Azure Blob store from a configured Azure builder.
    ///
    /// # Errors
    ///
    /// Returns an error when required Azure configuration is missing or invalid.
    pub fn new(builder: Azblob) -> Result<Self, opendal::Error> {
        Operator::new(builder).map(|operator| Self { operator })
    }
}

impl ObjectStorage for AzureBlob {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let body = match self.operator.read(key).await {
            Ok(body) => body.to_vec(),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_error(error)),
        };

        // Fetch real content type / timestamps so `get` and `head` agree.
        let metadata = match self.operator.stat(key).await {
            Ok(stat) => {
                let mut metadata = stat_metadata(key, &stat);
                metadata.size = u64::try_from(body.len()).map_err(|error| {
                    StorageError::Backend(format!("Azure blob size overflow: {error}"))
                })?;
                metadata
            }
            // The blob vanished between read and stat; report the read we made.
            Err(error) if error.kind() == ErrorKind::NotFound => ObjectMetadata {
                key: key.to_owned(),
                size: u64::try_from(body.len()).map_err(|error| {
                    StorageError::Backend(format!("Azure blob size overflow: {error}"))
                })?,
                content_type: None,
                last_modified: None,
                metadata: HashMap::new(),
            },
            Err(error) => return Err(storage_error(error)),
        };

        Ok(Some(StorageObject { body, metadata }))
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.operator
            .write(key, body)
            .await
            .map_err(storage_error)?;
        Ok(())
    }

    async fn put_with(
        &self,
        key: &str,
        body: Vec<u8>,
        options: PutOptions,
    ) -> Result<(), StorageError> {
        let mut request = self.operator.write_with(key, body);
        if let Some(content_type) = &options.content_type {
            request = request.content_type(content_type);
        }
        if !options.metadata.is_empty() {
            request = request.user_metadata(options.metadata);
        }
        request.await.map_err(storage_error)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self.operator.delete(key).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(storage_error(error)),
        }
    }

    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        // A zero limit yields an empty page, consistent with other backends;
        // the incoming cursor is echoed back so no listing progress is lost.
        if options.limit == Some(0) {
            return Ok(ListResult {
                objects: Vec::new(),
                cursor: options.cursor,
            });
        }

        let last_key = decode_blob_list_cursor(options.cursor.as_deref())?;

        // OpenDAL's azblob backend ignores `start_after`, so resume
        // client-side: list from the prefix start (the listing is
        // lexicographic), skip keys at or before the cursor, and read one
        // entry past the limit to learn whether more remain.
        let mut lister = self
            .operator
            .lister_with(options.prefix.as_deref().unwrap_or_default())
            .recursive(true)
            .await
            .map_err(storage_error)?;

        let over_fetch = options.limit.and_then(|limit| limit.checked_add(1));
        let mut objects = Vec::new();
        while let Some(entry) = lister.try_next().await.map_err(storage_error)? {
            let metadata = object_metadata(entry);
            if !is_after_cursor(&metadata.key, last_key.as_deref()) {
                continue;
            }
            objects.push(metadata);
            if over_fetch.is_some_and(|target| objects.len() == target) {
                break;
            }
        }

        let (objects, next_key) = clamp_page(objects, options.limit);
        let cursor = next_key
            .map(|last_key| encode_blob_list_cursor(&AzureBlobListCursor { last_key }))
            .transpose()?;
        Ok(ListResult { objects, cursor })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        match self.operator.stat(key).await {
            Ok(metadata) => Ok(Some(stat_metadata(key, &metadata))),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }
}

fn object_metadata(entry: Entry) -> ObjectMetadata {
    let (key, metadata) = entry.into_parts();
    stat_metadata(&key, &metadata)
}

fn stat_metadata(key: &str, metadata: &opendal::Metadata) -> ObjectMetadata {
    ObjectMetadata {
        key: key.to_owned(),
        size: metadata.content_length(),
        content_type: metadata.content_type().map(ToOwned::to_owned),
        last_modified: metadata
            .last_modified()
            .and_then(|timestamp| u64::try_from(timestamp.into_inner().as_second()).ok()),
        metadata: metadata.user_metadata().cloned().unwrap_or_default(),
    }
}

/// Whether `key` comes strictly after the cursor position in lexicographic
/// (byte-wise) order — the order Azure Blob listings use.
fn is_after_cursor(key: &str, last_key: Option<&str>) -> bool {
    last_key.is_none_or(|last| key.as_bytes() > last.as_bytes())
}

/// Truncate an over-fetched page to `limit` entries.
///
/// `objects` holds up to `limit + 1` entries; when it exceeds `limit`, the
/// page is truncated and the key of its final entry becomes the next cursor.
/// A page within the limit is final, so no cursor is returned.
fn clamp_page(
    mut objects: Vec<ObjectMetadata>,
    limit: Option<usize>,
) -> (Vec<ObjectMetadata>, Option<String>) {
    let Some(limit) = limit else {
        return (objects, None);
    };
    if objects.len() > limit {
        objects.truncate(limit);
        let cursor = objects.last().map(|object| object.key.clone());
        (objects, cursor)
    } else {
        (objects, None)
    }
}

fn storage_error(error: impl std::fmt::Display) -> StorageError {
    StorageError::Backend(error.to_string())
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
struct AzureBlobListCursor {
    last_key: String,
}

fn decode_blob_list_cursor(cursor: Option<&str>) -> Result<Option<String>, StorageError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };

    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|error| {
            StorageError::Backend(format!("invalid Azure blob cursor encoding: {error}"))
        })?;
    let cursor: AzureBlobListCursor = serde_json::from_slice(&bytes).map_err(|error| {
        StorageError::Backend(format!("invalid Azure blob cursor payload: {error}"))
    })?;
    Ok(Some(cursor.last_key))
}

fn encode_blob_list_cursor(cursor: &AzureBlobListCursor) -> Result<String, StorageError> {
    let payload = serde_json::to_vec(cursor).map_err(|error| {
        StorageError::Backend(format!("failed to serialize Azure blob cursor: {error}"))
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload))
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_page, decode_blob_list_cursor, encode_blob_list_cursor, is_after_cursor,
        AzureBlobListCursor, ObjectMetadata,
    };
    use std::collections::HashMap;

    fn object(key: &str) -> ObjectMetadata {
        ObjectMetadata {
            key: key.to_owned(),
            size: 0,
            content_type: None,
            last_modified: None,
            metadata: HashMap::new(),
        }
    }

    /// Simulate one `list` call against a lexicographically sorted backend
    /// listing, mirroring the streaming loop in `ObjectStorage::list`.
    fn run_page(
        keys: &[&str],
        last_key: Option<&str>,
        limit: Option<usize>,
    ) -> (Vec<String>, Option<String>) {
        let over_fetch = limit.and_then(|limit| limit.checked_add(1));
        let mut objects = Vec::new();
        for key in keys {
            if !is_after_cursor(key, last_key) {
                continue;
            }
            objects.push(object(key));
            if over_fetch.is_some_and(|target| objects.len() == target) {
                break;
            }
        }
        let (objects, cursor) = clamp_page(objects, limit);
        (objects.into_iter().map(|o| o.key).collect(), cursor)
    }

    #[test]
    fn is_after_cursor_without_cursor_accepts_everything() {
        assert!(is_after_cursor("a", None));
    }

    #[test]
    fn is_after_cursor_skips_keys_at_or_before_cursor() {
        assert!(!is_after_cursor("a", Some("b")));
        assert!(!is_after_cursor("b", Some("b")));
        assert!(is_after_cursor("c", Some("b")));
    }

    #[test]
    fn clamp_page_without_limit_returns_everything_and_no_cursor() {
        let (objects, cursor) = clamp_page(vec![object("a"), object("b")], None);
        assert_eq!(objects.len(), 2);
        assert_eq!(cursor, None);
    }

    #[test]
    fn clamp_page_truncates_over_fetched_page_and_sets_cursor() {
        let (objects, cursor) = clamp_page(vec![object("a"), object("b"), object("c")], Some(2));
        assert_eq!(
            objects.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(cursor.as_deref(), Some("b"));
    }

    #[test]
    fn clamp_page_final_page_has_no_cursor() {
        let (objects, cursor) = clamp_page(vec![object("a"), object("b")], Some(2));
        assert_eq!(objects.len(), 2);
        assert_eq!(cursor, None);
    }

    #[test]
    fn multi_page_walk_terminates_and_sees_all_keys_exactly_once() {
        let keys = ["a", "b", "c/1", "c/2", "d", "e", "f"];
        for limit in 1..=keys.len() + 1 {
            let mut seen = Vec::new();
            let mut cursor: Option<String> = None;
            let mut pages = 0;
            loop {
                pages += 1;
                assert!(pages <= keys.len() + 1, "pagination loop did not terminate");
                let (page, next) = run_page(&keys, cursor.as_deref(), Some(limit));
                seen.extend(page);
                match next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            let seen: Vec<&str> = seen.iter().map(String::as_str).collect();
            assert_eq!(seen, keys, "limit {limit} lost or duplicated keys");
        }
    }

    #[test]
    fn multi_page_walk_with_empty_listing_terminates() {
        let (page, cursor) = run_page(&[], None, Some(3));
        assert!(page.is_empty());
        assert_eq!(cursor, None);
    }

    #[test]
    fn cursor_round_trip() {
        let cursor = AzureBlobListCursor {
            last_key: "folder/last-key".to_owned(),
        };
        let encoded = encode_blob_list_cursor(&cursor).expect("cursor should encode");
        let decoded = decode_blob_list_cursor(Some(&encoded)).expect("cursor should decode");
        assert_eq!(decoded.as_deref(), Some(cursor.last_key.as_str()));
    }

    #[test]
    fn invalid_cursor_is_rejected() {
        let error = decode_blob_list_cursor(Some("not-a-valid-cursor"))
            .expect_err("invalid cursor should fail");
        assert!(error.to_string().contains("invalid Azure blob cursor"));
    }
}
