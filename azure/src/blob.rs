//! Azure Blob Storage implementation of [`ObjectStorage`].

use std::collections::HashMap;

use base64::Engine;
use futures_util::TryStreamExt;
pub use opendal::services::Azblob;
use opendal::{Entry, ErrorKind, Operator};
use serde::{Deserialize, Serialize};
use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, StorageError, StorageObject,
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
        match self.operator.read(key).await {
            Ok(body) => {
                let body = body.to_vec();
                let metadata = ObjectMetadata {
                    key: key.to_owned(),
                    size: u64::try_from(body.len()).map_err(|error| {
                        StorageError::Backend(format!("Azure blob size overflow: {error}"))
                    })?,
                    content_type: None,
                    last_modified: None,
                    metadata: HashMap::new(),
                };

                Ok(Some(StorageObject { body, metadata }))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.operator
            .write(key, body)
            .await
            .map_err(storage_error)?;
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
        if options.limit == Some(0) {
            return Err(StorageError::Backend(
                "list limit must be greater than zero".to_owned(),
            ));
        }

        let cursor = decode_blob_list_cursor(options.cursor.as_deref())?;
        let mut request = self
            .operator
            .lister_with(options.prefix.as_deref().unwrap_or_default())
            .recursive(true);
        if let Some(cursor) = &cursor {
            request = request.start_after(cursor);
        }
        if let Some(limit) = options.limit {
            request = request.limit(limit);
        }

        let mut lister = request.await.map_err(storage_error)?;
        let mut objects = Vec::new();
        while let Some(entry) = lister.try_next().await.map_err(storage_error)? {
            let metadata = object_metadata(entry);
            let next_cursor = metadata.key.clone();
            objects.push(metadata);

            if objects.len() == options.limit.unwrap_or(usize::MAX) {
                return Ok(ListResult {
                    objects,
                    cursor: Some(encode_blob_list_cursor(&AzureBlobListCursor {
                        last_key: next_cursor,
                    })?),
                });
            }
        }

        Ok(ListResult {
            objects,
            cursor: None,
        })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        match self.operator.stat(key).await {
            Ok(metadata) => Ok(Some(ObjectMetadata {
                key: key.to_owned(),
                size: metadata.content_length(),
                content_type: metadata.content_type().map(ToOwned::to_owned),
                last_modified: metadata
                    .last_modified()
                    .and_then(|timestamp| u64::try_from(timestamp.into_inner().as_second()).ok()),
                metadata: HashMap::new(),
            })),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }
}

fn object_metadata(entry: Entry) -> ObjectMetadata {
    let (key, metadata) = entry.into_parts();
    ObjectMetadata {
        key,
        size: metadata.content_length(),
        content_type: metadata.content_type().map(ToOwned::to_owned),
        last_modified: metadata
            .last_modified()
            .and_then(|timestamp| u64::try_from(timestamp.into_inner().as_second()).ok()),
        metadata: HashMap::new(),
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
    use super::{decode_blob_list_cursor, encode_blob_list_cursor, AzureBlobListCursor};

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
