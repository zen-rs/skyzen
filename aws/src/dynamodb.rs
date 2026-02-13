//! `DynamoDB` implementation of [`KeyValueStore`].
//!
//! Uses a `DynamoDB` table with a configurable partition key column.
//! Values are stored as binary attributes.

use std::collections::HashMap;

use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use skyzen_services::kv::{KeyValueStore, KvError};

/// A `DynamoDB`-backed key-value store.
///
/// Each item has a partition key (string) and a `value` attribute (binary).
/// The table must exist before use.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct DynamoKv {
    client: Client,
    table_name: String,
    key_attribute: String,
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
            .map_err(|e| KvError::Backend(e.to_string()))?;

        Ok(output.item().and_then(|item| {
            item.get("value")
                .and_then(|v| v.as_b().ok())
                .map(|b| b.as_ref().to_vec())
        }))
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.client
            .put_item()
            .table_name(&self.table_name)
            .item(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .item("value", AttributeValue::B(value.to_vec().into()))
            .send()
            .await
            .map_err(|e| KvError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.client
            .delete_item()
            .table_name(&self.table_name)
            .key(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .send()
            .await
            .map_err(|e| KvError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn list(&self, prefix: Option<&str>) -> Result<Vec<String>, KvError> {
        let mut keys = Vec::new();
        let mut exclusive_start_key: Option<HashMap<String, AttributeValue>> = None;

        loop {
            let mut scan = self.client.scan().table_name(&self.table_name);

            if let Some(p) = prefix {
                scan = scan
                    .filter_expression("begins_with(#pk, :prefix)")
                    .expression_attribute_names("#pk", &self.key_attribute)
                    .expression_attribute_values(":prefix", AttributeValue::S(p.to_owned()));
            }

            if let Some(ref start) = exclusive_start_key {
                scan = scan.set_exclusive_start_key(Some(start.clone()));
            }

            let output = scan
                .send()
                .await
                .map_err(|e| KvError::Backend(e.to_string()))?;

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
