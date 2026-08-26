//! `DynamoDB` implementation of [`KeyValueStore`].
//!
//! Uses a `DynamoDB` table with a configurable partition key column.
//! Values are stored as binary attributes.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use aws_smithy_types::error::display::DisplayErrorContext;
use skyzen_services::kv::{KeyValueStore, KvError};

/// A `DynamoDB`-backed key-value store.
///
/// Each item has a partition key (string) and a `value` attribute (binary).
/// The table must exist before use.
///
/// # Time-to-live
///
/// [`KeyValueStore::put_with_ttl`] stores the expiry as epoch seconds in a
/// configurable attribute (default `"expires_at"`, see
/// [`DynamoKv::with_ttl_attribute`]). Enable the `DynamoDB` TTL feature on the
/// table for that attribute so `DynamoDB` eventually deletes expired items.
/// Because `DynamoDB` TTL deletion is lazy (it can lag by up to ~48 hours),
/// `get` and `list` additionally filter out items whose expiry has already
/// passed.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct DynamoKv {
    client: Client,
    table_name: String,
    key_attribute: String,
    ttl_attribute: String,
}

impl DynamoKv {
    /// Create a new `DynamoKv` from an existing client, table name, and key attribute.
    pub fn new(
        client: Client,
        table_name: impl Into<String>,
        key_attribute: impl Into<String>,
    ) -> Self {
        Self {
            client,
            table_name: table_name.into(),
            key_attribute: key_attribute.into(),
            ttl_attribute: "expires_at".to_owned(),
        }
    }

    /// Create a new `DynamoKv` from environment configuration.
    ///
    /// Uses the default AWS SDK configuration loader and a default key attribute of `"pk"`.
    pub async fn from_env(table_name: impl Into<String>) -> Self {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = Client::new(&config);
        Self::new(client, table_name, "pk")
    }

    /// Set the attribute used to store expiry timestamps (default `"expires_at"`).
    ///
    /// The table's TTL feature should be enabled on this attribute so expired
    /// items are eventually deleted server-side.
    #[must_use]
    pub fn with_ttl_attribute(mut self, ttl_attribute: impl Into<String>) -> Self {
        self.ttl_attribute = ttl_attribute.into();
        self
    }

    /// The current time as seconds since the Unix epoch.
    fn now_epoch_seconds() -> Result<u64, KvError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|e| KvError::backend(format!("system clock before Unix epoch: {e}")))
    }
}

/// Map an AWS SDK error to a [`KvError::Backend`] with full error context.
///
/// [`DisplayErrorContext`] walks the whole error source chain, so the message
/// includes the service error code and message instead of just "service error".
fn backend_error<E>(err: E) -> KvError
where
    E: std::error::Error + Send + Sync + 'static,
{
    KvError::backend_with(DisplayErrorContext(&err).to_string(), err)
}

/// Compute the expiry timestamp for a TTL, rounding sub-second parts up.
///
/// Rejects zero and overflowing durations because an expiry in the past (or an
/// unrepresentable one) would silently drop the write.
fn expires_at_epoch_seconds(now_secs: u64, ttl: core::time::Duration) -> Result<u64, KvError> {
    if ttl.is_zero() {
        return Err(KvError::backend("TTL must be greater than zero"));
    }
    let mut seconds = ttl.as_secs();
    if ttl.subsec_nanos() > 0 {
        seconds = seconds
            .checked_add(1)
            .ok_or_else(|| KvError::backend("TTL overflows the epoch-seconds range"))?;
    }
    now_secs
        .checked_add(seconds)
        .ok_or_else(|| KvError::backend("TTL overflows the epoch-seconds range"))
}

/// Whether an item's TTL attribute marks it as already expired.
///
/// Items without the attribute (or with a malformed one) never expire
/// client-side; a malformed value is left for the server-side TTL sweeper.
fn item_expired(
    item: &HashMap<String, AttributeValue>,
    ttl_attribute: &str,
    now_secs: u64,
) -> bool {
    item.get(ttl_attribute)
        .and_then(|attr| attr.as_n().ok())
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|expires_at| expires_at <= now_secs)
}

/// Extract the binary `value` attribute from an item.
///
/// A present item without a binary `value` attribute indicates a schema
/// mismatch (e.g. the table is shared with non-KV data), which is reported as
/// an error instead of being conflated with "key absent".
fn extract_value(item: &HashMap<String, AttributeValue>, key: &str) -> Result<Vec<u8>, KvError> {
    item.get("value")
        .and_then(|v| v.as_b().ok())
        .map(|b| b.as_ref().to_vec())
        .ok_or_else(|| {
            KvError::backend(format!(
                "item for key {key:?} exists but its `value` attribute is missing or not binary"
            ))
        })
}

impl KeyValueStore for DynamoKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let output = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .send()
            .await
            .map_err(backend_error)?;

        let Some(item) = output.item() else {
            return Ok(None);
        };

        // DynamoDB TTL deletion is lazy, so expired items may still be present.
        if item_expired(item, &self.ttl_attribute, Self::now_epoch_seconds()?) {
            return Ok(None);
        }

        extract_value(item, key).map(Some)
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.client
            .put_item()
            .table_name(&self.table_name)
            .item(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .item("value", AttributeValue::B(value.to_vec().into()))
            .send()
            .await
            .map_err(backend_error)?;
        Ok(())
    }

    async fn put_with_ttl(
        &self,
        key: &str,
        value: &[u8],
        ttl: core::time::Duration,
    ) -> Result<(), KvError> {
        let expires_at = expires_at_epoch_seconds(Self::now_epoch_seconds()?, ttl)?;
        self.client
            .put_item()
            .table_name(&self.table_name)
            .item(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .item("value", AttributeValue::B(value.to_vec().into()))
            .item(
                &self.ttl_attribute,
                AttributeValue::N(expires_at.to_string()),
            )
            .send()
            .await
            .map_err(backend_error)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.client
            .delete_item()
            .table_name(&self.table_name)
            .key(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .send()
            .await
            .map_err(backend_error)?;
        Ok(())
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let now_secs = Self::now_epoch_seconds()?;
        let mut keys = Vec::new();
        let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> = None;

        loop {
            // Exclude items whose TTL already passed: DynamoDB deletes them lazily.
            let not_expired = "(attribute_not_exists(#ttl) OR #ttl > :now)";
            let mut scan = self
                .client
                .scan()
                .table_name(&self.table_name)
                .expression_attribute_names("#ttl", &self.ttl_attribute)
                .expression_attribute_values(":now", AttributeValue::N(now_secs.to_string()));

            if let Some(p) = prefix {
                scan = scan
                    .filter_expression(format!("begins_with(#pk, :prefix) AND {not_expired}"))
                    .expression_attribute_names("#pk", &self.key_attribute)
                    .expression_attribute_values(":prefix", AttributeValue::S(p.to_owned()));
            } else {
                scan = scan.filter_expression(not_expired);
            }

            if let Some(ref start) = exclusive_start_key {
                scan = scan.set_exclusive_start_key(Some(start.clone()));
            }

            let output = scan.send().await.map_err(backend_error)?;

            if let Some(items) = output.items {
                for item in &items {
                    if let Some(key_attr) = item.get(&self.key_attribute) {
                        if let Ok(s) = key_attr.as_s() {
                            keys.push(s.clone());
                        }
                    }
                }
            }

            exclusive_start_key = output.last_evaluated_key;
            if exclusive_start_key.is_none() {
                break;
            }
        }

        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::{expires_at_epoch_seconds, extract_value, item_expired, AttributeValue};
    use core::time::Duration;
    use std::collections::HashMap;

    #[test]
    fn expires_at_adds_whole_seconds() {
        assert_eq!(
            expires_at_epoch_seconds(100, Duration::from_mins(1)).unwrap(),
            160
        );
    }

    #[test]
    fn expires_at_rounds_subsecond_ttl_up() {
        assert_eq!(
            expires_at_epoch_seconds(100, Duration::from_millis(1500)).unwrap(),
            102
        );
        assert_eq!(
            expires_at_epoch_seconds(100, Duration::from_millis(10)).unwrap(),
            101
        );
    }

    #[test]
    fn expires_at_rejects_zero_ttl() {
        assert!(expires_at_epoch_seconds(100, Duration::ZERO).is_err());
    }

    #[test]
    fn expires_at_rejects_overflow() {
        assert!(expires_at_epoch_seconds(u64::MAX, Duration::from_secs(1)).is_err());
    }

    fn item_with_ttl(ttl_value: AttributeValue) -> HashMap<String, AttributeValue> {
        let mut item = HashMap::new();
        item.insert("expires_at".to_owned(), ttl_value);
        item.insert(
            "value".to_owned(),
            AttributeValue::B(b"payload".to_vec().into()),
        );
        item
    }

    #[test]
    fn item_without_ttl_attribute_never_expires() {
        let item = HashMap::new();
        assert!(!item_expired(&item, "expires_at", u64::MAX));
    }

    #[test]
    fn item_with_past_ttl_is_expired() {
        let item = item_with_ttl(AttributeValue::N("100".to_owned()));
        assert!(item_expired(&item, "expires_at", 100));
        assert!(item_expired(&item, "expires_at", 101));
    }

    #[test]
    fn item_with_future_ttl_is_not_expired() {
        let item = item_with_ttl(AttributeValue::N("100".to_owned()));
        assert!(!item_expired(&item, "expires_at", 99));
    }

    #[test]
    fn item_with_malformed_ttl_is_kept() {
        let item = item_with_ttl(AttributeValue::S("not-a-number".to_owned()));
        assert!(!item_expired(&item, "expires_at", u64::MAX));
    }

    #[test]
    fn extract_value_returns_binary_payload() {
        let item = item_with_ttl(AttributeValue::N("100".to_owned()));
        assert_eq!(extract_value(&item, "k").unwrap(), b"payload".to_vec());
    }

    #[test]
    fn extract_value_reports_missing_attribute_as_error() {
        let item: HashMap<String, AttributeValue> = HashMap::new();
        let error = extract_value(&item, "k").unwrap_err();
        assert!(error.to_string().contains("missing or not binary"));
    }

    #[test]
    fn extract_value_reports_non_binary_attribute_as_error() {
        let mut item = HashMap::new();
        item.insert("value".to_owned(), AttributeValue::S("text".to_owned()));
        assert!(extract_value(&item, "k").is_err());
    }
}
