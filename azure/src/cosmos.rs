//! Azure Cosmos DB implementation of [`KeyValueStore`].
//!
//! Uses a Cosmos DB container with a configurable partition key.
//! Values are stored as JSON documents with an `id` field (the key)
//! and a `value` field (base64-encoded bytes).

use azure_core::http::StatusCode;
use azure_data_cosmos::clients::ContainerClient;
use base64::Engine;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use skyzen_services::kv::{KeyValueStore, KvError};

/// Document schema for key-value items in Cosmos DB.
#[derive(Debug, Serialize, Deserialize)]
struct KvDocument {
    /// The document ID (used as the key).
    id: String,
    /// The base64-encoded value.
    value: String,
    /// Partition key value (same as id for simple KV use cases).
    partition_key: String,
}

/// An Azure Cosmos DB-backed key-value store.
///
/// Uses a single container where each item has an `id` (the key)
/// and a `value` (base64-encoded binary data).
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Clone)]
pub struct CosmosKv {
    container: ContainerClient,
}

impl std::fmt::Debug for CosmosKv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CosmosKv").finish_non_exhaustive()
    }
}

impl CosmosKv {
    /// Create a new `CosmosKv` from an existing container client.
    ///
    /// The container should be configured with `/partition_key` as the partition key path.
    #[must_use]
    pub const fn new(container: ContainerClient) -> Self {
        Self { container }
    }
}

/// Encode bytes to base64 for Cosmos DB storage.
fn encode_value(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Decode base64 string back to bytes.
fn decode_value(encoded: &str) -> Result<Vec<u8>, KvError> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| KvError::Backend(format!("base64 decode error: {e}")))
}

impl KeyValueStore for CosmosKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        // Partition key requires 'static, so we must use an owned String
        match self
            .container
            .read_item::<KvDocument>(key.to_string(), key, None)
            .await
        {
            Ok(response) => {
                let doc = response.into_model().map_err(az_err)?;
                decode_value(&doc.value).map(Some)
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(az_err(e)),
        }
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        let doc = KvDocument {
            id: key.to_owned(),
            value: encode_value(value),
            partition_key: key.to_owned(),
        };

        self.container
            .upsert_item(key.to_string(), doc, None)
            .await
            .map_err(az_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        match self
            .container
            .delete_item(key.to_string(), key, None)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(az_err(e)),
        }
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let query = prefix.map_or_else(
            || "SELECT c.id FROM c".to_owned(),
            |p| {
                format!(
                    "SELECT c.id FROM c WHERE STARTSWITH(c.id, '{}')",
                    p.replace('\'', "''")
                )
            },
        );

        // Use empty partition key `()` for cross-partition queries
        let mut pager = self
            .container
            .query_items::<serde_json::Value>(query, (), None)
            .map_err(az_err)?;

        let mut keys = Vec::new();
        while let Some(item) = pager.try_next().await.map_err(az_err)? {
            if let Some(id) = item.get("id").and_then(serde_json::Value::as_str) {
                keys.push(id.to_owned());
            }
        }

        Ok(keys)
    }
}

/// Convert an Azure error to a [`KvError`].
///
/// Takes ownership to match `Result<_, Error>::map_err` signature.
#[allow(clippy::needless_pass_by_value)]
fn az_err(e: azure_core::Error) -> KvError {
    KvError::Backend(e.to_string())
}

/// Check if an Azure error is a 404 Not Found.
fn is_not_found(err: &azure_core::Error) -> bool {
    err.http_status() == Some(StatusCode::NotFound)
}
