//! Azure Blob Storage implementation of [`ObjectStorage`].

use std::collections::HashMap;
use std::sync::Arc;

use azure_core::http::headers::CONTENT_TYPE;
use azure_core::http::StatusCode;
use azure_storage_blob::models::{BlobClientGetPropertiesResultHeaders, BlobItemInternal};
use azure_storage_blob::BlobContainerClient;
use futures_util::TryStreamExt;
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

/// Extract the blob name from a [`BlobItemInternal`], falling back to an empty string.
fn blob_name(blob: &BlobItemInternal) -> String {
    blob.name
        .as_ref()
        .and_then(|n| n.content.clone())
        .unwrap_or_default()
}

/// Extract content length from a [`BlobItemInternal`], defaulting to 0.
fn blob_content_length(blob: &BlobItemInternal) -> u64 {
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
        let mut pager = self.container.list_blobs(None).map_err(az_err)?;

        let mut objects = Vec::new();

        while let Some(blob) = pager.try_next().await.map_err(az_err)? {
            let name = blob_name(&blob);

            // Apply prefix filter if specified
            if let Some(ref prefix) = options.prefix {
                if !name.starts_with(prefix.as_str()) {
                    continue;
                }
            }

            objects.push(ObjectMetadata {
                key: name,
                size: blob_content_length(&blob),
                content_type: None,
                last_modified: None,
                metadata: HashMap::new(),
            });

            // Stop at the limit
            if let Some(limit) = options.limit {
                if objects.len() >= limit {
                    break;
                }
            }
        }

        Ok(ListResult {
            objects,
            cursor: None,
        })
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

/// Check if an Azure error is a 404 Not Found.
fn is_not_found(err: &azure_core::Error) -> bool {
    err.http_status() == Some(StatusCode::NotFound)
}
