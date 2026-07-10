//! Azure Cosmos DB implementation of [`KeyValueStore`].
//!
//! Uses a Cosmos DB container with a configurable partition key.
//! Values are stored as JSON documents with an `id` field (the key)
//! and a `value` field (base64-encoded bytes).

use azure_data_cosmos::{clients::ContainerClient, CosmosError, FeedScope};
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
        match self.container.read_item(key.to_owned(), key, None).await {
            Ok(response) => {
                let doc = response
                    .into_model::<KvDocument>()
                    .map_err(kv_backend_err)?;
                decode_value(&doc.value).map(Some)
            }
            Err(error) if is_not_found(&error) => Ok(None),
            Err(e) => Err(kv_backend_err(e)),
        }
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        let doc = KvDocument {
            id: key.to_owned(),
            value: encode_value(value),
            partition_key: key.to_owned(),
        };

        self.container
            .upsert_item(key.to_owned(), key, doc, None)
            .await
            .map_err(kv_backend_err)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        match self.container.delete_item(key.to_owned(), key, None).await {
            Ok(_) => Ok(()),
            Err(error) if is_not_found(&error) => Ok(()),
            Err(e) => Err(kv_backend_err(e)),
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

        // Query the full container because key prefixes can span logical partitions.
        let mut pager = self
            .container
            .query_items::<serde_json::Value>(query, FeedScope::full_container(), None)
            .await
            .map_err(kv_backend_err)?;

        let mut keys = Vec::new();
        while let Some(item) = pager.try_next().await.map_err(kv_backend_err)? {
            if let Some(id) = item.get("id").and_then(serde_json::Value::as_str) {
                keys.push(id.to_owned());
            }
        }

        Ok(keys)
    }
}

fn kv_backend_err<E: std::fmt::Display>(e: E) -> KvError {
    KvError::Backend(e.to_string())
}

fn is_not_found(error: &CosmosError) -> bool {
    u16::from(error.status().status_code()) == 404
}
