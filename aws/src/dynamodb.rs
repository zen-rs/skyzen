//! `DynamoDB` implementation of [`KeyValueStore`].
//!
//! Uses a `DynamoDB` table with a configurable partition key column.
//! Values are stored as binary attributes.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::operation::update_item::UpdateItemError;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use aws_smithy_types::error::display::DisplayErrorContext;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use skyzen_services::kv::{KeyValueStore, KvError, KvListOptions, KvListResult};

use crate::errors::{categorize, AwsErrorCategory};

/// The attribute holding a key's binary payload.
///
/// `value` is a `DynamoDB` **reserved word**, so every expression that mentions it must do so
/// through an expression-attribute-name placeholder (`#v`); writing it literally is a runtime
/// `ValidationException`.
const VALUE_ATTRIBUTE: &str = "value";

/// The condition fragment that excludes an item whose TTL has already passed.
///
/// `DynamoDB`'s TTL sweeper is lazy — it can lag by up to ~48 hours — so "expired" and "deleted"
/// are different states, and every read and precondition in this backend treats the first as the
/// second. An item carrying no TTL attribute never expires, which is what `attribute_not_exists`
/// admits.
const NOT_EXPIRED: &str = "(attribute_not_exists(#ttl) OR #ttl > :now)";

/// Precondition for a write onto a key that must currently hold nothing.
///
/// Expired-but-unswept counts as holding nothing, so a lock abandoned by a crashed holder can be
/// re-acquired the moment its TTL passes rather than whenever `DynamoDB` gets around to the sweep.
const ABSENT_OR_EXPIRED: &str = "attribute_not_exists(#pk) OR #ttl <= :now";

/// Precondition for a write onto a key that must still hold a known value.
const VALUE_MATCHES_AND_LIVE: &str =
    "#v = :expected AND (attribute_not_exists(#ttl) OR #ttl > :now)";

/// Precondition for re-arming the TTL of a key that must currently hold a value.
const PRESENT_AND_LIVE: &str =
    "attribute_exists(#pk) AND (attribute_not_exists(#ttl) OR #ttl > :now)";

/// The attributes an existence check reads: enough to answer, and none of the payload.
const KEY_AND_TTL_PROJECTION: &str = "#pk, #ttl";

/// How many times [`KeyValueStore::increment`] re-reads and retries before giving up.
///
/// Each attempt is one lost race against another writer. Eight is far past what uncontended or
/// mildly contended traffic needs, and stopping there turns pathological contention into a
/// [`KvError::Conflict`] the caller can back off on instead of an unbounded spin.
const MAX_INCREMENT_ATTEMPTS: u32 = 8;

/// Which attributes a read asks `DynamoDB` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Projection {
    /// Every attribute, including the binary payload.
    WholeItem,
    /// Only the key and the TTL attribute, which is all an existence check needs.
    KeyAndTtl,
}

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
/// every read *and every conditional write* additionally treats an item whose expiry has passed as
/// absent, so the portable contract ("once `ttl` elapses the key is absent") holds on AWS.
///
/// # Read consistency
///
/// `DynamoDB` reads are eventually consistent by default, so a write followed immediately by a
/// read can observe the stale value — unlike Redis or Cosmos point reads.
/// [`with_consistent_reads`](DynamoKv::with_consistent_reads) opts
/// [`get`](KeyValueStore::get) and [`exists`](KeyValueStore::exists) into `ConsistentRead` at
/// twice the read capacity. [`increment`](KeyValueStore::increment) always reads consistently
/// regardless: a read-modify-write loop cannot converge on a stale read.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Debug, Clone)]
pub struct DynamoKv {
    client: Client,
    table_name: String,
    key_attribute: String,
    ttl_attribute: String,
    consistent_reads: bool,
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
            consistent_reads: false,
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

    /// Read with `ConsistentRead`, so a value read back after a write is never the stale one.
    ///
    /// Off by default, matching `DynamoDB`'s own default. Turning it on doubles the read capacity
    /// each [`get`](KeyValueStore::get) and [`exists`](KeyValueStore::exists) consumes, which is
    /// the trade a session store or an idempotency key wants and a warm cache does not.
    #[must_use]
    pub const fn with_consistent_reads(mut self, consistent_reads: bool) -> Self {
        self.consistent_reads = consistent_reads;
        self
    }

    /// Rebuild a scan's `ExclusiveStartKey` from a continuation token.
    ///
    /// The token is the partition key of the last item the previous page examined, and the table's
    /// only key attribute is that partition key, so the item key round-trips exactly.
    fn start_key_for(&self, cursor: &str) -> HashMap<String, AttributeValue> {
        HashMap::from([(
            self.key_attribute.clone(),
            AttributeValue::S(cursor.to_owned()),
        )])
    }

    /// The current time as seconds since the Unix epoch.
    fn now_epoch_seconds() -> Result<u64, KvError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|e| KvError::backend(format!("system clock before Unix epoch: {e}")))
    }

    /// Read one item, reporting an item whose TTL has already passed as absent.
    async fn read_item(
        &self,
        key: &str,
        consistent: bool,
        projection: Projection,
    ) -> Result<Option<HashMap<String, AttributeValue>>, KvError> {
        let mut request = self
            .client
            .get_item()
            .table_name(&self.table_name)
            .key(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .consistent_read(consistent);

        if projection == Projection::KeyAndTtl {
            // Both attributes are caller-named and could collide with a reserved word, so the
            // projection names them through placeholders.
            request = request
                .projection_expression(KEY_AND_TTL_PROJECTION)
                .expression_attribute_names("#pk", &self.key_attribute)
                .expression_attribute_names("#ttl", &self.ttl_attribute);
        }

        let output = request.send().await.map_err(sdk_error)?;
        let Some(item) = output.item else {
            return Ok(None);
        };

        // DynamoDB TTL deletion is lazy, so expired items may still be present.
        if item_expired(&item, &self.ttl_attribute, Self::now_epoch_seconds()?) {
            return Ok(None);
        }

        Ok(Some(item))
    }

    /// Write `new` under `key` only if the stored value still matches `expected`.
    ///
    /// `expected` is `None` to require that the key hold nothing, which includes an item whose TTL
    /// has passed but that `DynamoDB` has not swept yet. `expires_at` re-writes the TTL attribute;
    /// `None` writes the item without one, because `PutItem` replaces the whole item.
    ///
    /// Returns `false` when the precondition no longer held: `ConditionalCheckFailedException` is
    /// the ordinary outcome of a lost optimistic update, so it never surfaces as an error.
    async fn conditional_put(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: &[u8],
        expires_at: Option<u64>,
    ) -> Result<bool, KvError> {
        let now_secs = Self::now_epoch_seconds()?;

        let mut request = self
            .client
            .put_item()
            .table_name(&self.table_name)
            .item(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .item(VALUE_ATTRIBUTE, AttributeValue::B(new.to_vec().into()))
            // `#ttl` and `:now` are referenced by both preconditions below.
            .expression_attribute_names("#ttl", &self.ttl_attribute)
            .expression_attribute_values(":now", AttributeValue::N(now_secs.to_string()));

        if let Some(expires_at) = expires_at {
            request = request.item(
                &self.ttl_attribute,
                AttributeValue::N(expires_at.to_string()),
            );
        }

        // Only the placeholders the chosen expression actually uses may be declared — DynamoDB
        // rejects a request carrying an unreferenced expression attribute name or value.
        request = match expected {
            None => request
                .condition_expression(ABSENT_OR_EXPIRED)
                .expression_attribute_names("#pk", &self.key_attribute),
            Some(expected) => request
                .condition_expression(VALUE_MATCHES_AND_LIVE)
                .expression_attribute_names("#v", VALUE_ATTRIBUTE)
                .expression_attribute_values(
                    ":expected",
                    AttributeValue::B(expected.to_vec().into()),
                ),
        };

        match request.send().await {
            Ok(_) => Ok(true),
            Err(err)
                if err
                    .as_service_error()
                    .is_some_and(PutItemError::is_conditional_check_failed_exception) =>
            {
                Ok(false)
            }
            Err(err) => Err(sdk_error(err)),
        }
    }
}

/// Map an AWS SDK error to a [`KvError`], reading its service error code first.
///
/// Throttling and credential rejections become their own variants so a handler can back off or
/// give up without matching on message text; everything else keeps the full SDK message.
/// [`DisplayErrorContext`] walks the whole error source chain, so that message includes the
/// service error code and message instead of just "service error".
///
/// `ConditionalCheckFailedException` deliberately stays in the `Backend` category here: every
/// conditional write in this backend inspects the *typed* error at its call site and reports the
/// lost race as `Ok(false)`, so it never reaches this function.
fn sdk_error<E>(err: E) -> KvError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match categorize(&err) {
        AwsErrorCategory::Throttled => KvError::Throttled { retry_after: None },
        AwsErrorCategory::Unauthorized => KvError::Unauthorized,
        AwsErrorCategory::Backend => {
            KvError::backend_with(DisplayErrorContext(&err).to_string(), err)
        }
    }
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

/// The epoch-second expiry an item carries, when it carries a well-formed one.
///
/// Items without the attribute — or with a malformed one — have no client-side expiry; a malformed
/// value is left for the server-side TTL sweeper rather than being guessed at.
fn item_expires_at(item: &HashMap<String, AttributeValue>, ttl_attribute: &str) -> Option<u64> {
    item.get(ttl_attribute)
        .and_then(|attr| attr.as_n().ok())
        .and_then(|n| n.parse::<u64>().ok())
}

/// Whether an item's TTL attribute marks it as already expired.
fn item_expired(
    item: &HashMap<String, AttributeValue>,
    ttl_attribute: &str,
    now_secs: u64,
) -> bool {
    item_expires_at(item, ttl_attribute).is_some_and(|expires_at| expires_at <= now_secs)
}

/// Extract the binary `value` attribute from an item.
///
/// A present item without a binary `value` attribute indicates a schema
/// mismatch (e.g. the table is shared with non-KV data), which is reported as
/// an error instead of being conflated with "key absent".
fn extract_value(item: &HashMap<String, AttributeValue>, key: &str) -> Result<Vec<u8>, KvError> {
    item.get(VALUE_ATTRIBUTE)
        .and_then(|v| v.as_b().ok())
        .map(|b| b.as_ref().to_vec())
        .ok_or_else(|| {
            KvError::backend(format!(
                "item for key {key:?} exists but its `{VALUE_ATTRIBUTE}` attribute is missing or \
                 not binary"
            ))
        })
}

/// Read a stored counter, treating an absent (or expired) key as zero.
///
/// A value that is not a decimal integer is a caller mistake — `INCRBY` on a non-numeric key fails
/// on Redis too, and `InMemoryKv` reports the same [`KvError::Decode`] — so it surfaces rather
/// than silently resetting the counter to zero.
fn counter_value(stored: Option<&[u8]>) -> Result<i64, KvError> {
    let Some(stored) = stored else {
        return Ok(0);
    };
    let text = core::str::from_utf8(stored)
        .map_err(|error| KvError::Decode(format!("counter is not valid UTF-8: {error}")))?;
    text.parse()
        .map_err(|error| KvError::Decode(format!("counter {text:?} is not an integer: {error}")))
}

impl KeyValueStore for DynamoKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        let Some(item) = self
            .read_item(key, self.consistent_reads, Projection::WholeItem)
            .await?
        else {
            return Ok(None);
        };

        extract_value(&item, key).map(Some)
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.client
            .put_item()
            .table_name(&self.table_name)
            .item(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .item(VALUE_ATTRIBUTE, AttributeValue::B(value.to_vec().into()))
            .send()
            .await
            .map_err(sdk_error)?;
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
            .item(VALUE_ATTRIBUTE, AttributeValue::B(value.to_vec().into()))
            .item(
                &self.ttl_attribute,
                AttributeValue::N(expires_at.to_string()),
            )
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// Take the key only if nothing holds it, using a `DynamoDB` condition expression.
    ///
    /// An item whose TTL has passed but that `DynamoDB` has not swept counts as unheld, so a lock
    /// left behind by a crashed holder is re-acquirable the moment it expires.
    async fn put_if_absent(&self, key: &str, value: &[u8]) -> Result<bool, KvError> {
        self.conditional_put(key, None, value, None).await
    }

    /// Swap the value under `key` with a condition expression on its current bytes.
    ///
    /// The winner owns the key outright: like `SET` on Redis and like `InMemoryKv`, a swap re-arms
    /// no expiry, so the item is written without a TTL attribute.
    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        new: &[u8],
    ) -> Result<bool, KvError> {
        self.conditional_put(key, expected, new, None).await
    }

    /// Add `delta` to a counter by reading it and writing it back under a condition.
    ///
    /// `DynamoDB`'s own `ADD` action counts in `N` attributes, but this backend stores every value
    /// as binary so a counter stays readable through [`get`](KeyValueStore::get); the compare-and-
    /// swap loop is what keeps that representation atomic. The read leg is always strongly
    /// consistent — a read-modify-write cannot converge on a stale read — and whatever expiry the
    /// counter already carried is preserved, so an incremented rate-limit window still closes on
    /// schedule.
    ///
    /// # Errors
    ///
    /// [`KvError::Decode`] if the stored value is not a decimal integer or the result overflows an
    /// `i64`, and [`KvError::Conflict`] if the swap lost its race
    /// [`MAX_INCREMENT_ATTEMPTS`] times running.
    async fn increment(&self, key: &str, delta: i64) -> Result<i64, KvError> {
        for _ in 0..MAX_INCREMENT_ATTEMPTS {
            let item = self.read_item(key, true, Projection::WholeItem).await?;
            let stored = item
                .as_ref()
                .map(|item| extract_value(item, key))
                .transpose()?;

            let updated = counter_value(stored.as_deref())?
                .checked_add(delta)
                .ok_or_else(|| KvError::Decode(format!("counter {key:?} overflowed an i64")))?;

            // A counter that had already expired starts a fresh, unexpiring one.
            let expires_at = item
                .as_ref()
                .and_then(|item| item_expires_at(item, &self.ttl_attribute));

            if self
                .conditional_put(
                    key,
                    stored.as_deref(),
                    updated.to_string().as_bytes(),
                    expires_at,
                )
                .await?
            {
                return Ok(updated);
            }
        }

        Err(KvError::Conflict)
    }

    /// Re-arm the expiry of a key that still holds a value, with `UpdateItem`.
    ///
    /// Returns `false` when no live item holds the key — including one whose TTL has already
    /// passed, which the portable contract already treats as absent.
    async fn expire(&self, key: &str, ttl: core::time::Duration) -> Result<bool, KvError> {
        let now_secs = Self::now_epoch_seconds()?;
        let expires_at = expires_at_epoch_seconds(now_secs, ttl)?;

        let result = self
            .client
            .update_item()
            .table_name(&self.table_name)
            .key(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .update_expression("SET #ttl = :expires_at")
            .condition_expression(PRESENT_AND_LIVE)
            .expression_attribute_names("#pk", &self.key_attribute)
            .expression_attribute_names("#ttl", &self.ttl_attribute)
            .expression_attribute_values(":expires_at", AttributeValue::N(expires_at.to_string()))
            .expression_attribute_values(":now", AttributeValue::N(now_secs.to_string()))
            .send()
            .await;

        match result {
            Ok(_) => Ok(true),
            Err(err)
                if err
                    .as_service_error()
                    .is_some_and(UpdateItemError::is_conditional_check_failed_exception) =>
            {
                Ok(false)
            }
            Err(err) => Err(sdk_error(err)),
        }
    }

    /// Answer from a `GetItem` that projects only the key and the TTL attribute.
    ///
    /// The default would read the whole binary payload and throw it away, which on a table holding
    /// large values is most of the request's cost.
    async fn exists(&self, key: &str) -> Result<bool, KvError> {
        Ok(self
            .read_item(key, self.consistent_reads, Projection::KeyAndTtl)
            .await?
            .is_some())
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        self.client
            .delete_item()
            .table_name(&self.table_name)
            .key(&self.key_attribute, AttributeValue::S(key.to_owned()))
            .send()
            .await
            .map_err(sdk_error)?;
        Ok(())
    }

    /// List one page of keys using `DynamoDB`'s own `ExclusiveStartKey` cursor.
    ///
    /// A scan page can come back empty while still having more table left — the `limit` bounds
    /// items *examined*, and the TTL filter is applied after — so the loop keeps scanning until
    /// the requested number of keys is collected or the table is exhausted. The cursor handed to
    /// the caller is the partition key of the last examined item, which is enough to rebuild
    /// `ExclusiveStartKey` because the table's only key attribute is that partition key.
    ///
    /// `DynamoDB` applies a filter expression *after* reading, so the read capacity a prefixed
    /// listing consumes is proportional to the table, not to the matching prefix.
    async fn list(&self, options: KvListOptions) -> Result<KvListResult, KvError> {
        let now_secs = Self::now_epoch_seconds()?;
        let mut keys = Vec::new();
        let mut exclusive_start_key = options
            .cursor
            .as_deref()
            .map(|cursor| self.start_key_for(cursor));

        loop {
            // Exclude items whose TTL already passed: DynamoDB deletes them lazily.
            let mut scan = self
                .client
                .scan()
                .table_name(&self.table_name)
                .expression_attribute_names("#ttl", &self.ttl_attribute)
                .expression_attribute_values(":now", AttributeValue::N(now_secs.to_string()));

            if let Some(prefix) = options.prefix.as_deref() {
                scan = scan
                    .filter_expression(format!("begins_with(#pk, :prefix) AND {NOT_EXPIRED}"))
                    .expression_attribute_names("#pk", &self.key_attribute)
                    .expression_attribute_values(":prefix", AttributeValue::S(prefix.to_owned()));
            } else {
                scan = scan.filter_expression(NOT_EXPIRED);
            }

            if let Some(limit) = options.limit.and_then(|limit| i32::try_from(limit).ok()) {
                scan = scan.limit(limit);
            }
            scan = scan.set_exclusive_start_key(exclusive_start_key);

            let output = scan.send().await.map_err(sdk_error)?;

            for item in output.items.iter().flatten() {
                if let Some(Ok(key)) = item.get(&self.key_attribute).map(AttributeValue::as_s) {
                    keys.push(key.clone());
                }
            }

            exclusive_start_key = output.last_evaluated_key;
            let Some(start_key) = &exclusive_start_key else {
                return Ok(KvListResult { keys, cursor: None });
            };

            if options.limit.is_some_and(|limit| keys.len() >= limit) {
                let cursor = start_key
                    .get(&self.key_attribute)
                    .and_then(|attr| attr.as_s().ok())
                    .cloned();
                return Ok(KvListResult { keys, cursor });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        counter_value, expires_at_epoch_seconds, extract_value, item_expired, item_expires_at,
        AttributeValue, ABSENT_OR_EXPIRED, KEY_AND_TTL_PROJECTION, NOT_EXPIRED, PRESENT_AND_LIVE,
        VALUE_ATTRIBUTE, VALUE_MATCHES_AND_LIVE,
    };
    use core::time::Duration;
    use skyzen_services::kv::KvError;
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
            VALUE_ATTRIBUTE.to_owned(),
            AttributeValue::B(b"payload".to_vec().into()),
        );
        item
    }

    #[test]
    fn item_without_ttl_attribute_never_expires() {
        let item = HashMap::new();
        assert!(!item_expired(&item, "expires_at", u64::MAX));
        assert_eq!(item_expires_at(&item, "expires_at"), None);
    }

    #[test]
    fn item_with_past_ttl_is_expired() {
        let item = item_with_ttl(AttributeValue::N("100".to_owned()));
        assert!(item_expired(&item, "expires_at", 100));
        assert!(item_expired(&item, "expires_at", 101));
        assert_eq!(item_expires_at(&item, "expires_at"), Some(100));
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
        assert_eq!(item_expires_at(&item, "expires_at"), None);
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
        item.insert(
            VALUE_ATTRIBUTE.to_owned(),
            AttributeValue::S("text".to_owned()),
        );
        assert!(extract_value(&item, "k").is_err());
    }

    #[test]
    fn counter_value_treats_an_absent_key_as_zero() {
        assert_eq!(counter_value(None).unwrap(), 0);
        assert_eq!(counter_value(Some(b"41")).unwrap(), 41);
        assert_eq!(counter_value(Some(b"-7")).unwrap(), -7);
    }

    #[test]
    fn counter_value_reports_a_non_integer_payload_as_a_decode_error() {
        // Same taxonomy as `InMemoryKv`, so portable code sees one behaviour on both backends.
        assert!(matches!(
            counter_value(Some(b"alice")).unwrap_err(),
            KvError::Decode(_)
        ));
        assert!(matches!(
            counter_value(Some(&[0xFF, 0xFE])).unwrap_err(),
            KvError::Decode(_)
        ));
        assert!(matches!(
            counter_value(Some(b"1.5")).unwrap_err(),
            KvError::Decode(_)
        ));
    }

    /// Every expression this backend sends must reference `value` only through a placeholder:
    /// `value` is a `DynamoDB` reserved word, and spelling it out is a `ValidationException` that no
    /// offline test would otherwise catch.
    #[test]
    fn no_expression_spells_out_the_reserved_word_value() {
        for expression in [
            ABSENT_OR_EXPIRED,
            VALUE_MATCHES_AND_LIVE,
            PRESENT_AND_LIVE,
            NOT_EXPIRED,
            KEY_AND_TTL_PROJECTION,
        ] {
            assert!(
                !expression.contains(VALUE_ATTRIBUTE),
                "{expression:?} names the reserved word `{VALUE_ATTRIBUTE}` directly"
            );
        }
        assert!(VALUE_MATCHES_AND_LIVE.starts_with("#v = :expected"));
    }

    /// The "still live" clause is written once and reused, so the scan filter and the conditional
    /// write can never disagree about what "expired" means.
    #[test]
    fn every_liveness_clause_shares_one_definition() {
        assert!(VALUE_MATCHES_AND_LIVE.ends_with(NOT_EXPIRED));
        assert!(PRESENT_AND_LIVE.ends_with(NOT_EXPIRED));
        // The absent-side precondition is the complement: an item is unheld when it has no key
        // attribute at all, or when its expiry has already passed.
        assert!(ABSENT_OR_EXPIRED.contains("attribute_not_exists(#pk)"));
        assert!(ABSENT_OR_EXPIRED.contains("#ttl <= :now"));
    }
}
