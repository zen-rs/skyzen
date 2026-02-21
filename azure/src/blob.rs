//! Azure Blob Storage implementation of [`ObjectStorage`].

use std::collections::HashMap;
use std::sync::Arc;

use azure_core::http::headers::CONTENT_TYPE;
use azure_core::http::StatusCode;
use azure_storage_blob::models::{
    BlobClientGetPropertiesResultHeaders, BlobContainerClientListBlobsOptions, BlobItem,
};
use azure_storage_blob::BlobContainerClient;
use base64::Engine;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use skyzen_services::storage::{
    ListOptions, ListResult, ObjectMetadata, ObjectStorage, StorageError, StorageObject,
};

/// An Azure Blob Storage-backed object store.
///
/// Wraps the Azure SDK's Blob Container client to implement [`ObjectStorage`].
///
/// `BlobContainerClient` does not implement `Clone`, so it is wrapped in an `Arc`.
pub struct AzureBlob {
    container: Arc<BlobContainerClient>,
}

impl Clone for AzureBlob {
    fn clone(&self) -> Self {
        Self {
            container: Arc::clone(&self.container),
        }
    }
}

impl std::fmt::Debug for AzureBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureBlob").finish_non_exhaustive()
    }
}

impl AzureBlob {
    /// Create a new `AzureBlob` from an existing container client.
    #[must_use]
    pub fn new(container: BlobContainerClient) -> Self {
        Self {
            container: Arc::new(container),
        }
    }
}

/// Extract the blob name from a [`BlobItem`], falling back to an empty string.
fn blob_name(blob: &BlobItem) -> String {
    blob.name
        .as_ref()
        .and_then(|n| n.content.clone())
        .unwrap_or_default()
}

/// Extract content length from a [`BlobItem`], defaulting to 0.
fn blob_content_length(blob: &BlobItem) -> u64 {
    blob.properties
        .as_ref()
        .and_then(|p| p.content_length)
        .unwrap_or(0)
}

impl ObjectStorage for AzureBlob {
    async fn get(&self, key: &str) -> Result<Option<StorageObject>, StorageError> {
        let blob = self.container.blob_client(key);
        let result = blob.download(None).await;

        match result {
            Ok(response) => {
                let body = response
                    .into_body()
                    .collect()
                    .await
                    .map_err(az_err)?
                    .to_vec();

                let metadata = ObjectMetadata {
                    key: key.to_owned(),
                    #[allow(clippy::cast_possible_truncation)]
                    size: body.len() as u64,
                    content_type: None,
                    last_modified: None,
                    metadata: HashMap::new(),
                };

                Ok(Some(StorageObject { body, metadata }))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(az_err(e)),
        }
    }

    async fn put(&self, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        let blob = self.container.blob_client(key);
        #[allow(clippy::cast_possible_truncation)]
        let len = body.len() as u64;
        let data = azure_core::http::RequestContent::from(body);
        blob.upload(data, true, len, None).await.map_err(az_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let blob = self.container.blob_client(key);
        match blob.delete(None).await {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(az_err(e)),
        }
    }

    async fn list(&self, options: ListOptions) -> Result<ListResult, StorageError> {
        if options.limit == Some(0) {
            return Err(StorageError::Backend(
                "list limit must be greater than zero".to_owned(),
            ));
        }

        let incoming_cursor = decode_blob_list_cursor(options.cursor.as_deref())?;
        let mut pager = self
            .container
            .list_blobs(Some(BlobContainerClientListBlobsOptions {
                prefix: options.prefix.clone(),
                marker: incoming_cursor.marker.clone(),
                maxresults: options.limit.map(limit_to_i32).transpose()?,
                ..Default::default()
            }))
            .map_err(az_err)?;

        let mut current_marker = incoming_cursor.marker;
        let mut offset_in_page = 0usize;

        for _ in 0..incoming_cursor.offset {
            if pager.try_next().await.map_err(az_err)?.is_none() {
                return Err(StorageError::Backend(
                    "invalid Azure blob cursor: offset exceeds available items".to_owned(),
                ));
            }

            advance_blob_cursor(
                &mut current_marker,
                &mut offset_in_page,
                pager.continuation().cloned().map(String::from),
            )?;
        }

        let limit = options.limit.unwrap_or(usize::MAX);
        let mut objects = Vec::new();

        loop {
            let Some(blob) = pager.try_next().await.map_err(az_err)? else {
                return Ok(ListResult {
                    objects,
                    cursor: None,
                });
            };

            advance_blob_cursor(
                &mut current_marker,
                &mut offset_in_page,
                pager.continuation().cloned().map(String::from),
            )?;

            objects.push(ObjectMetadata {
                key: blob_name(&blob),
                size: blob_content_length(&blob),
                content_type: None,
                last_modified: None,
                metadata: HashMap::new(),
            });

            if objects.len() >= limit {
                return Ok(ListResult {
                    objects,
                    cursor: Some(encode_blob_list_cursor(&AzureBlobListCursor {
                        marker: current_marker.clone(),
                        offset: offset_in_page,
                    })?),
                });
            }
        }
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMetadata>, StorageError> {
        let blob = self.container.blob_client(key);
        match blob.get_properties(None).await {
            Ok(response) => {
                let size = response.content_length().map_err(az_err)?.unwrap_or(0);
                let last_modified = response
                    .last_modified()
                    .map_err(az_err)?
                    .and_then(|ts| u64::try_from(ts.unix_timestamp()).ok());
                let metadata = response.metadata().map_err(az_err)?;
                let content_type = response.headers().get_optional_string(&CONTENT_TYPE);

                Ok(Some(ObjectMetadata {
                    key: key.to_owned(),
                    size,
                    content_type,
                    last_modified,
                    metadata,
                }))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(az_err(e)),
        }
    }
}

/// Convert an Azure error to a [`StorageError`].
///
/// Takes ownership to match `Result<_, Error>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn az_err(e: azure_core::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}

#[derive(Debug, Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
struct AzureBlobListCursor {
    marker: Option<String>,
    offset: usize,
}

fn decode_blob_list_cursor(cursor: Option<&str>) -> Result<AzureBlobListCursor, StorageError> {
    let Some(cursor) = cursor else {
        return Ok(AzureBlobListCursor::default());
    };

    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|e| StorageError::Backend(format!("invalid Azure blob cursor encoding: {e}")))?;

    serde_json::from_slice(&bytes)
        .map_err(|e| StorageError::Backend(format!("invalid Azure blob cursor payload: {e}")))
}

fn encode_blob_list_cursor(cursor: &AzureBlobListCursor) -> Result<String, StorageError> {
    let payload = serde_json::to_vec(cursor).map_err(|e| {
        StorageError::Backend(format!("failed to serialize Azure blob cursor: {e}"))
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload))
}

fn advance_blob_cursor(
    current_marker: &mut Option<String>,
    offset_in_page: &mut usize,
    marker_after_item: Option<String>,
) -> Result<(), StorageError> {
    if *current_marker != marker_after_item {
        *current_marker = marker_after_item;
        *offset_in_page = 0;
    }
    *offset_in_page = offset_in_page
        .checked_add(1)
        .ok_or_else(|| StorageError::Backend("Azure blob cursor offset overflow".to_owned()))?;
    Ok(())
}

fn limit_to_i32(limit: usize) -> Result<i32, StorageError> {
    i32::try_from(limit)
        .map_err(|_| StorageError::Backend(format!("list limit too large for Azure Blob: {limit}")))
}

/// Check if an Azure error is a 404 Not Found.
fn is_not_found(err: &azure_core::Error) -> bool {
    err.http_status() == Some(StatusCode::NotFound)
}

#[cfg(test)]
mod tests {
    use super::{
        advance_blob_cursor, decode_blob_list_cursor, encode_blob_list_cursor, AzureBlobListCursor,
    };

    #[test]
    fn cursor_round_trip() {
        let cursor = AzureBlobListCursor {
            marker: Some("opaque-marker".to_owned()),
            offset: 42,
        };
        let encoded = encode_blob_list_cursor(&cursor).expect("cursor should encode");
        let decoded = decode_blob_list_cursor(Some(&encoded)).expect("cursor should decode");
        assert_eq!(decoded, cursor);
    }

    #[test]
    fn invalid_cursor_is_rejected() {
        let err = decode_blob_list_cursor(Some("not-a-valid-cursor"))
            .expect_err("invalid cursor should fail");
        assert!(err.to_string().contains("invalid Azure blob cursor"));
    }

    #[test]
    fn cursor_offset_resets_on_marker_change() {
        let mut marker = None;
        let mut offset = 0usize;

        advance_blob_cursor(&mut marker, &mut offset, None).expect("advance should work");
        assert_eq!(marker, None);
        assert_eq!(offset, 1);

        advance_blob_cursor(&mut marker, &mut offset, Some("next-page".to_owned()))
            .expect("advance should work");
        assert_eq!(marker, Some("next-page".to_owned()));
        assert_eq!(offset, 1);
    }
}
