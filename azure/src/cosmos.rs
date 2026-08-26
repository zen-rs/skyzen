//! Azure Cosmos DB implementation of [`KeyValueStore`].
//!
//! Uses a Cosmos DB container with a configurable partition key.
//! Values are stored as JSON documents with an `id` field (the key)
//! and a `value` field (base64-encoded bytes).

use azure_data_cosmos::{clients::ContainerClient, CosmosError, FeedScope, Query};
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
    /// Per-document time-to-live in seconds (Cosmos DB's `ttl` field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ttl: Option<i32>,
}

/// An Azure Cosmos DB-backed key-value store.
///
/// Uses a single container where each item has an `id` (the key)
/// and a `value` (base64-encoded binary data).
///
/// # Time-to-live
///
/// [`KeyValueStore::put_with_ttl`] sets the per-document `ttl` field in
/// seconds. The container must have TTL enabled (`DefaultTimeToLive` set,
/// e.g. `-1`) for Cosmos DB to honor it; otherwise the field is ignored and
/// the document never expires.
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
        .map_err(|e| KvError::backend(format!("base64 decode error: {e}")))
}

/// Convert a TTL duration to whole seconds for Cosmos DB's `ttl` field.
///
/// Sub-second parts are rounded up. Zero durations are rejected (Cosmos
/// requires a positive TTL), as are durations beyond the `i32` range Cosmos
/// accepts for the field.
fn ttl_seconds(ttl: core::time::Duration) -> Result<i32, KvError> {
    if ttl.is_zero() {
        return Err(KvError::backend("TTL must be greater than zero"));
    }
    let mut seconds = ttl.as_secs();
    if ttl.subsec_nanos() > 0 {
        seconds = seconds.saturating_add(1);
    }
    // Cosmos DB stores `ttl` as a 32-bit signed integer.
    i32::try_from(seconds).map_err(|_| {
        KvError::backend(format!(
            "TTL of {seconds} s exceeds the Cosmos DB maximum of {} s",
            i32::MAX
        ))
    })
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
            ttl: None,
        };

        self.container
            .upsert_item(key.to_owned(), key, doc, None)
            .await
            .map_err(kv_backend_err)?;
        Ok(())
    }

    /// Store a value with a per-document TTL (Cosmos DB's `ttl` field, in seconds).
    ///
    /// The container must have TTL enabled (a `DefaultTimeToLive` set, e.g.
    /// `-1` for "on, no default expiry") for the per-document `ttl` field to
    /// take effect; on containers without it the field is ignored by Cosmos.
    async fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: core::time::Duration,
    ) -> Result<(), KvError> {
        let doc = KvDocument {
            id: key.to_owned(),
            value: encode_value(value),
            partition_key: key.to_owned(),
            ttl: Some(ttl_seconds(ttl)?),
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
        // Parameterize the prefix instead of interpolating it into the query
        // text, so no prefix content can alter the query (Cosmos SQL string
        // escaping uses backslashes, which naive quoting mishandles).
        let query = match prefix {
            None => Query::from("SELECT c.id FROM c"),
            Some(p) => Query::from("SELECT c.id FROM c WHERE STARTSWITH(c.id, @prefix)")
                .with_parameter("@prefix", p)
                .map_err(kv_backend_err)?,
        };

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

fn kv_backend_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> KvError {
    KvError::backend_with(e.to_string(), e)
}

fn is_not_found(error: &CosmosError) -> bool {
    u16::from(error.status().status_code()) == 404
}

#[cfg(test)]
mod tests {
    use super::{decode_value, encode_value, ttl_seconds};
    use core::time::Duration;

    #[test]
    fn value_encoding_round_trips() {
        let payload = [0_u8, 1, 2, 255];
        assert_eq!(decode_value(&encode_value(&payload)).unwrap(), payload);
    }

    #[test]
    fn ttl_seconds_rejects_zero() {
        assert!(ttl_seconds(Duration::ZERO).is_err());
    }

    #[test]
    fn ttl_seconds_rounds_subsecond_up() {
        assert_eq!(ttl_seconds(Duration::from_millis(1500)).unwrap(), 2);
        assert_eq!(ttl_seconds(Duration::from_millis(10)).unwrap(), 1);
    }

    #[test]
    fn ttl_seconds_converts_whole_seconds() {
        assert_eq!(ttl_seconds(Duration::from_hours(1)).unwrap(), 3600);
    }

    #[test]
    fn ttl_seconds_rejects_values_beyond_i32() {
        assert!(ttl_seconds(Duration::from_secs(u64::from(u32::MAX))).is_err());
    }
}
