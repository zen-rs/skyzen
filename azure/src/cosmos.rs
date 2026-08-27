//! Azure Cosmos DB implementation of [`KeyValueStore`].
//!
//! Values are stored as JSON documents whose `id` is the key and whose `value` is the
//! base64-encoded bytes. The container's own partition key definition decides where the partition
//! value goes, so this backend binds to a container that already exists rather than requiring one
//! shaped a particular way.

use core::{num::NonZeroU32, time::Duration};
use std::sync::Arc;

use azure_core::credentials::{Secret, TokenCredential};
use azure_data_cosmos::{
    clients::ContainerClient,
    feed::ContinuationToken,
    models::{PatchInstructions, PatchOperation},
    options::{
        FeedOptions, ItemWriteOptions, MaxItemCountHint, PatchItemOptions, Precondition,
        QueryOptions, RoutingStrategy,
    },
    AccountEndpoint, AccountReference, CosmosClient, CosmosError, FeedScope, Query,
};
use base64::Engine as _;
use futures_util::TryStreamExt as _;
use serde::{Deserialize, Serialize};
use skyzen_services::kv::{KeyValueStore, KvError, KvListOptions, KvListResult};

use crate::status::{classify, AzureStatus};

/// The environment variable [`CosmosKv::from_env`] reads the account endpoint from.
const ENDPOINT_ENV: &str = "AZURE_COSMOS_ENDPOINT";

/// The environment variable [`CosmosKv::from_env`] reads the account key from.
const KEY_ENV: &str = "AZURE_COSMOS_KEY";

/// The document field holding the base64-encoded value.
const VALUE_FIELD: &str = "value";

/// Cosmos DB's own per-document time-to-live field.
const TTL_FIELD: &str = "ttl";

/// How many times [`KeyValueStore::increment`] re-reads and retries after losing a race.
///
/// Cosmos has no server-side counter for a value it does not model as a number, so the counter is
/// an `If-Match` guarded read-modify-write. Bounded so a hot key fails loudly instead of spinning.
const INCREMENT_ATTEMPTS: usize = 8;

/// How the partition key value of a document is chosen.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PartitionStrategy {
    /// The partition key value is the key itself, so every document is its own logical partition.
    ///
    /// The default, and the right one for a container used purely as a key-value store: writes
    /// spread evenly and a point read never crosses a partition. Listing has to fan out across
    /// partitions, which is what makes [`KeyValueStore::list`] a full-container query.
    #[default]
    SameAsId,

    /// Every document this store writes lands in one named partition.
    ///
    /// The right one for a container shared with other data, where this store's keys belong to a
    /// tenant or a namespace: listing then reads a single partition instead of fanning out. The
    /// partition is limited to 20 GB, as every Cosmos logical partition is.
    Fixed(String),
}

/// Where a document's partition key value goes, learned from the container itself.
#[derive(Debug, Clone)]
struct DocumentLayout {
    /// The document field the container partitions on, or `None` when it partitions on `/id` and
    /// the id is already that field.
    partition_field: Option<String>,
    /// How the partition value is chosen.
    strategy: PartitionStrategy,
    /// Whether the container has time-to-live enabled, which per-document `ttl` needs.
    ttl_enabled: bool,
}

impl DocumentLayout {
    /// The partition key value for `key`.
    fn partition_value<'a>(&'a self, key: &'a str) -> &'a str {
        match &self.strategy {
            PartitionStrategy::SameAsId => key,
            PartitionStrategy::Fixed(partition) => partition,
        }
    }

    /// The document to store under `key`.
    fn document(
        &self,
        key: &str,
        value: &[u8],
        ttl: Option<i32>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut document = serde_json::Map::new();
        document.insert("id".to_owned(), key.into());
        document.insert(VALUE_FIELD.to_owned(), encode_value(value).into());

        if let Some(field) = &self.partition_field {
            document.insert(field.clone(), self.partition_value(key).into());
        }

        if let Some(ttl) = ttl {
            document.insert(TTL_FIELD.to_owned(), ttl.into());
        }

        document
    }
}

/// Read a container's partition key definition into a [`DocumentLayout`].
///
/// The container is the source of truth: whatever path it partitions on is the field this backend
/// writes the partition value into, so it binds to a container that already exists instead of
/// requiring one shaped `/partition_key`.
fn document_layout(
    paths: &[impl AsRef<str>],
    strategy: PartitionStrategy,
    ttl_enabled: bool,
) -> Result<DocumentLayout, KvError> {
    let [path] = paths else {
        return Err(KvError::Unsupported(
            "a container with hierarchical (multi-path) partition keys cannot back a key-value \
             store; use a container with a single partition key path",
        ));
    };

    let field = path.as_ref().strip_prefix('/').ok_or_else(|| {
        KvError::backend(format!(
            "the container partitions on {:?}, which is not a document path",
            path.as_ref()
        ))
    })?;

    if field.is_empty() || field.contains('/') {
        return Err(KvError::backend(format!(
            "the container partitions on {:?}; this backend stores flat documents and can only \
             fill a top-level partition key path",
            path.as_ref()
        )));
    }

    if field == "id" {
        if let PartitionStrategy::Fixed(partition) = &strategy {
            return Err(KvError::backend(format!(
                "the container partitions on /id, so each document's partition value is its own \
                 key; it cannot also be fixed at {partition:?}"
            )));
        }

        return Ok(DocumentLayout {
            partition_field: None,
            strategy,
            ttl_enabled,
        });
    }

    if field == VALUE_FIELD || field == TTL_FIELD {
        return Err(KvError::backend(format!(
            "the container partitions on {:?}, which is the field this backend stores its own \
             {field} in",
            path.as_ref()
        )));
    }

    Ok(DocumentLayout {
        partition_field: Some(field.to_owned()),
        strategy,
        ttl_enabled,
    })
}

/// Builds a [`CosmosKv`] bound to a container.
///
/// Building reads the container's definition once, so a partition key path or a time-to-live
/// setting this store cannot work with fails at startup rather than on the first write.
pub struct CosmosKvBuilder {
    /// The container to bind to.
    container: ContainerClient,
    /// How the partition key value is chosen.
    strategy: PartitionStrategy,
}

impl core::fmt::Debug for CosmosKvBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CosmosKvBuilder")
            .field("strategy", &self.strategy)
            .finish_non_exhaustive()
    }
}

impl CosmosKvBuilder {
    /// Choose how each document's partition key value is derived.
    ///
    /// Defaults to [`PartitionStrategy::SameAsId`].
    #[must_use]
    pub fn with_partition_strategy(mut self, strategy: PartitionStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Read the container's definition and bind to it.
    ///
    /// # Errors
    ///
    /// [`KvError::Unsupported`] when the container's partition key is one flat documents cannot
    /// fill, [`KvError::backend`] when the container cannot be read or its partition key path
    /// conflicts with the requested [`PartitionStrategy`].
    pub async fn build(self) -> Result<CosmosKv, KvError> {
        let properties = self
            .container
            .read(None)
            .await
            .map_err(cosmos_error)?
            .into_model()
            .map_err(cosmos_error)?;

        let layout = document_layout(
            properties.partition_key.paths(),
            self.strategy,
            !properties.default_ttl.is_forever(),
        )?;

        Ok(CosmosKv {
            container: self.container,
            layout,
        })
    }
}

/// An Azure Cosmos DB-backed key-value store.
///
/// Each key is one document: the key is its `id`, the value its base64-encoded `value`, and the
/// partition value goes wherever the container's own partition key path says.
///
/// # Time-to-live
///
/// [`KeyValueStore::put_with_ttl`] and [`KeyValueStore::expire`] set the document's `ttl` field.
/// Cosmos honours it only on a container that has time-to-live enabled, so binding to a container
/// that does not reports [`KvError::Unsupported`] rather than storing a value that would never
/// expire.
///
/// # Listing
///
/// [`KeyValueStore::list`] is a query, and a query's cursor belongs to the query that produced it:
/// resume a listing with the same [`KvListOptions::prefix`] it started with. Under
/// [`PartitionStrategy::SameAsId`] the query fans out across every partition, which is the cost of
/// spreading keys evenly.
///
/// Cloning is cheap — the underlying client uses `Arc` internally.
#[derive(Clone)]
pub struct CosmosKv {
    /// The bound container.
    container: ContainerClient,
    /// What the container's definition says about document shape.
    layout: DocumentLayout,
}

impl core::fmt::Debug for CosmosKv {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CosmosKv")
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

impl CosmosKv {
    /// Start building a store bound to `container`.
    #[must_use]
    pub const fn builder(container: ContainerClient) -> CosmosKvBuilder {
        CosmosKvBuilder {
            container,
            strategy: PartitionStrategy::SameAsId,
        }
    }

    /// Bind to a container using the account endpoint and key in the environment.
    ///
    /// Reads `AZURE_COSMOS_ENDPOINT` and `AZURE_COSMOS_KEY`. The client it builds expresses no
    /// regional preference; an application that needs multi-region routing builds its own
    /// [`CosmosClient`] and uses [`builder`](Self::builder).
    ///
    /// # Errors
    ///
    /// [`KvError::Backend`] when either variable is unset, when the account cannot be reached, or
    /// when the database or container does not exist.
    pub async fn from_env(database: &str, container: &str) -> Result<Self, KvError> {
        let endpoint = std::env::var(ENDPOINT_ENV).map_err(|error| {
            KvError::backend_with(
                format!("{ENDPOINT_ENV} is not set to a Cosmos DB account endpoint"),
                error,
            )
        })?;
        let key = std::env::var(KEY_ENV).map_err(|error| {
            KvError::backend_with(
                format!("{KEY_ENV} is not set to a Cosmos DB account key"),
                error,
            )
        })?;

        Self::from_account(
            AccountReference::with_authentication_key(
                account_endpoint(&endpoint)?,
                Secret::from(key),
            ),
            database,
            container,
        )
        .await
    }

    /// Bind to a container using an Entra ID token credential.
    ///
    /// The credential comes from `azure_identity` — `DeveloperToolsCredential` for local
    /// development, `ManagedIdentityCredential` in Azure.
    ///
    /// # Errors
    ///
    /// [`KvError::Backend`] when the endpoint is not a URL, when the account cannot be reached, or
    /// when the database or container does not exist.
    pub async fn with_credential(
        endpoint: &str,
        credential: Arc<dyn TokenCredential>,
        database: &str,
        container: &str,
    ) -> Result<Self, KvError> {
        Self::from_account(
            AccountReference::with_credential(account_endpoint(endpoint)?, credential),
            database,
            container,
        )
        .await
    }

    /// Build a client for `account` and bind to one of its containers.
    async fn from_account(
        account: AccountReference,
        database: &str,
        container: &str,
    ) -> Result<Self, KvError> {
        let client = CosmosClient::builder()
            // No regional preference: the SDK picks, and an application that cares builds its own
            // client rather than having a region guessed for it here.
            .build(account, RoutingStrategy::PreferredRegions(Vec::new()))
            .await
            .map_err(cosmos_error)?;

        let container = client
            .database_client(database)
            .container_client(container)
            .await
            .map_err(cosmos_error)?;

        Self::builder(container).build().await
    }

    /// Read a document's value and entity tag.
    async fn read(&self, key: &str) -> Result<Option<(Vec<u8>, String)>, KvError> {
        match self
            .container
            .read_item(self.layout.partition_value(key).to_owned(), key, None)
            .await
        {
            Ok(response) => {
                let etag = response
                    .headers()
                    .etag()
                    .map(ToString::to_string)
                    .ok_or_else(|| {
                        KvError::backend(
                            "Cosmos returned a document with no ETag, which no conditional write \
                             can be built on",
                        )
                    })?;
                let document: StoredValue = response.into_model().map_err(cosmos_error)?;
                Ok(Some((decode_value(&document.value)?, etag)))
            }
            Err(error) if is_absent(&error) => Ok(None),
            Err(error) => Err(cosmos_error(error)),
        }
    }

    /// Create a document, reporting whether it was created or already existed.
    async fn create(&self, key: &str, value: &[u8], ttl: Option<i32>) -> Result<bool, KvError> {
        match self
            .container
            .create_item(
                self.layout.partition_value(key).to_owned(),
                key,
                self.layout.document(key, value, ttl),
                None,
            )
            .await
        {
            Ok(_) => Ok(true),
            // The document is already there, which is the ordinary outcome of losing a
            // create-if-absent race rather than a failure.
            Err(error) if is_conflict(&error) => Ok(false),
            Err(error) => Err(cosmos_error(error)),
        }
    }

    /// Replace a document only while its entity tag still matches.
    async fn replace_if_unchanged(
        &self,
        key: &str,
        value: &[u8],
        etag: &str,
    ) -> Result<bool, KvError> {
        match self
            .container
            .replace_item(
                self.layout.partition_value(key).to_owned(),
                key,
                self.layout.document(key, value, None),
                Some(ItemWriteOptions::default().with_precondition(Precondition::if_match(etag))),
            )
            .await
        {
            Ok(_) => Ok(true),
            // The document changed between the read and this write: an ordinary lost race.
            Err(error) if is_precondition_failed(&error) || is_absent(&error) => Ok(false),
            Err(error) => Err(cosmos_error(error)),
        }
    }

    /// Store a value, with or without a per-document time-to-live.
    async fn upsert(&self, key: &str, value: &[u8], ttl: Option<i32>) -> Result<(), KvError> {
        self.container
            .upsert_item(
                self.layout.partition_value(key).to_owned(),
                key,
                self.layout.document(key, value, ttl),
                None,
            )
            .await
            .map_err(cosmos_error)?;
        Ok(())
    }
}

/// Parse an account endpoint, naming what it should have looked like.
fn account_endpoint(endpoint: &str) -> Result<AccountEndpoint, KvError> {
    endpoint.parse().map_err(|error| {
        KvError::backend_with(
            format!(
                "{endpoint:?} is not a Cosmos DB account endpoint; it looks like \
                 https://myaccount.documents.azure.com/"
            ),
            error,
        )
    })
}

/// The document fields this backend reads back.
///
/// Everything else the document carries — the partition field, Cosmos' own system properties — is
/// ignored, so a container shared with other data stays readable.
#[derive(Debug, Deserialize, Serialize)]
struct StoredValue {
    /// The base64-encoded value.
    value: String,
}

/// Encode bytes to base64 for Cosmos DB storage.
fn encode_value(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Decode a base64 string back to bytes.
fn decode_value(encoded: &str) -> Result<Vec<u8>, KvError> {
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| KvError::backend_with("base64 decode error", error))
}

/// Convert a TTL duration to whole seconds for Cosmos DB's `ttl` field.
///
/// Sub-second parts are rounded up. Zero durations are rejected (Cosmos requires a positive TTL),
/// as are durations beyond the `i32` range Cosmos accepts for the field.
fn ttl_seconds(ttl: Duration) -> Result<i32, KvError> {
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

/// Read a stored value as the decimal counter [`KeyValueStore::increment`] keeps.
///
/// The same form the Redis backend stores a counter in, so a counter written by one and read by
/// the other means the same thing.
fn counter_value(key: &str, value: &[u8]) -> Result<i64, KvError> {
    core::str::from_utf8(value)
        .ok()
        .and_then(|text| text.trim().parse::<i64>().ok())
        .ok_or_else(|| {
            KvError::backend(format!(
                "the value stored under {key:?} is not a decimal counter, so it cannot be \
                 incremented"
            ))
        })
}

/// The status Cosmos answered a failed request with.
fn status_of(error: &CosmosError) -> u16 {
    u16::from(error.status().status_code())
}

/// Whether the error says the document is not there.
fn is_absent(error: &CosmosError) -> bool {
    classify(status_of(error)) == AzureStatus::Absent
}

/// Whether the error says a document with that id already exists.
fn is_conflict(error: &CosmosError) -> bool {
    classify(status_of(error)) == AzureStatus::Conflict
}

/// Whether the error says the `If-Match` no longer held.
fn is_precondition_failed(error: &CosmosError) -> bool {
    classify(status_of(error)) == AzureStatus::PreconditionFailed
}

/// Map a Cosmos error onto the portable taxonomy, keeping its source chain.
///
/// A throttled request carries Cosmos' own `x-ms-retry-after-ms`, which is the wait the service
/// computed rather than a guess.
fn cosmos_error(error: CosmosError) -> KvError {
    match classify(status_of(&error)) {
        AzureStatus::Throttled => KvError::Throttled {
            retry_after: error
                .response()
                .and_then(|response| response.headers().retry_after_ms)
                .map(Duration::from_millis),
        },
        AzureStatus::Unauthorized => KvError::Unauthorized,
        AzureStatus::Conflict | AzureStatus::PreconditionFailed => KvError::Conflict,
        _ => KvError::backend_with(error.to_string(), error),
    }
}

impl KeyValueStore for CosmosKv {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvError> {
        Ok(self.read(key).await?.map(|(value, _)| value))
    }

    async fn put(&self, key: &str, value: &[u8]) -> Result<(), KvError> {
        self.upsert(key, value, None).await
    }

    /// Store a value with a per-document TTL (Cosmos DB's `ttl` field, in seconds).
    ///
    /// The container must have time-to-live enabled for Cosmos to honour the field, which was
    /// checked when this store was built.
    async fn put_with_ttl(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), KvError> {
        if !self.layout.ttl_enabled {
            return Err(KvError::Unsupported(
                "this Cosmos container has time-to-live disabled, so a per-document ttl would be \
                 ignored and the value would never expire; enable DefaultTimeToLive on the \
                 container",
            ));
        }

        self.upsert(key, value, Some(ttl_seconds(ttl)?)).await
    }

    /// Create the document only if no document with that id exists in its partition.
    async fn put_if_absent(&self, key: &str, value: &[u8]) -> Result<bool, KvError> {
        self.create(key, value, None).await
    }

    /// Replace the value only while it still matches `expected`.
    ///
    /// Cosmos guards a conditional write with the document's entity tag, so this reads the
    /// document, compares the bytes, and replaces under `If-Match`. A document that changed
    /// between the read and the write loses the race and reports `Ok(false)`.
    async fn compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: &[u8],
    ) -> Result<bool, KvError> {
        let Some(expected) = expected else {
            return self.create(key, value, None).await;
        };

        let Some((stored, etag)) = self.read(key).await? else {
            return Ok(false);
        };

        if stored != expected {
            return Ok(false);
        }

        self.replace_if_unchanged(key, value, &etag).await
    }

    /// Add `delta` to the decimal counter stored under `key`.
    ///
    /// Cosmos has no counter for a value it does not model as a number, so this is an `If-Match`
    /// guarded read-modify-write: a lost race is retried, and a key contended past
    /// [`INCREMENT_ATTEMPTS`] rounds reports [`KvError::Conflict`] rather than spinning.
    async fn increment(&self, key: &str, delta: i64) -> Result<i64, KvError> {
        for _ in 0..INCREMENT_ATTEMPTS {
            let applied = match self.read(key).await? {
                None => {
                    // An absent key counts as zero, and creating it is what makes the first
                    // increment safe against a second caller doing the same.
                    let next = delta;
                    self.create(key, next.to_string().as_bytes(), None)
                        .await?
                        .then_some(next)
                }
                Some((stored, etag)) => {
                    let next =
                        counter_value(key, &stored)?
                            .checked_add(delta)
                            .ok_or_else(|| {
                                KvError::backend(format!(
                            "incrementing the counter under {key:?} by {delta} overflows a 64-bit \
                             integer"
                        ))
                            })?;
                    self.replace_if_unchanged(key, next.to_string().as_bytes(), &etag)
                        .await?
                        .then_some(next)
                }
            };

            if let Some(next) = applied {
                return Ok(next);
            }
        }

        Err(KvError::Conflict)
    }

    /// Set the document's `ttl` field, leaving its value alone.
    ///
    /// Reports `false` when no document with that id exists.
    async fn expire(&self, key: &str, ttl: Duration) -> Result<bool, KvError> {
        if !self.layout.ttl_enabled {
            return Err(KvError::Unsupported(
                "this Cosmos container has time-to-live disabled, so setting a document's ttl \
                 would not expire it; enable DefaultTimeToLive on the container",
            ));
        }

        let instructions = PatchInstructions::new().with_operation(PatchOperation::set(
            format!("/{TTL_FIELD}"),
            ttl_seconds(ttl)?.into(),
        ));

        match self
            .container
            .patch_item(
                self.layout.partition_value(key).to_owned(),
                key,
                instructions,
                Some(PatchItemOptions::default()),
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(error) if is_absent(&error) => Ok(false),
            Err(error) => Err(cosmos_error(error)),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), KvError> {
        match self
            .container
            .delete_item(self.layout.partition_value(key).to_owned(), key, None)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if is_absent(&error) => Ok(()),
            Err(error) => Err(cosmos_error(error)),
        }
    }

    /// List one page of keys, resuming from Cosmos' own continuation token.
    ///
    /// The cursor belongs to the query that produced it, so a resumed page must be asked for with
    /// the same [`KvListOptions::prefix`]; Cosmos refuses a token from another query rather than
    /// answering the wrong one. [`KvListOptions::limit`] is a target: a page is filled to at least
    /// the limit and overshoots by at most one Cosmos page, which is what the trait allows so a
    /// cursor never skips keys. The last page may hand back a cursor whose own page turns out
    /// empty, as every token-based pager does.
    async fn list(&self, options: KvListOptions) -> Result<KvListResult, KvError> {
        // Parameterize the prefix instead of interpolating it into the query text, so no prefix
        // content can alter the query (Cosmos SQL string escaping uses backslashes, which naive
        // quoting mishandles).
        let query = match options.prefix.as_deref() {
            None => Query::from("SELECT c.id FROM c"),
            Some(prefix) => Query::from("SELECT c.id FROM c WHERE STARTSWITH(c.id, @prefix)")
                .with_parameter("@prefix", prefix)
                .map_err(cosmos_error)?,
        };

        let mut feed = FeedOptions::default();
        if let Some(limit) = options.limit.and_then(|limit| u32::try_from(limit).ok()) {
            if let Some(limit) = NonZeroU32::new(limit) {
                feed = feed.with_max_item_count(MaxItemCountHint::Limit(limit));
            }
        }
        if let Some(cursor) = options.cursor {
            feed = feed.with_continuation_token(ContinuationToken::from_string(cursor));
        }

        // Keys spread across every logical partition under the default strategy, so the query has
        // to read the whole container; a fixed partition still reads only its own.
        let scope = match &self.layout.strategy {
            PartitionStrategy::SameAsId => FeedScope::full_container(),
            PartitionStrategy::Fixed(partition) => FeedScope::partition(partition.clone()),
        };

        let mut pages = self
            .container
            .query_items::<KeyRow>(
                query,
                scope,
                Some(QueryOptions::default().with_feed_options(feed)),
            )
            .await
            .map_err(cosmos_error)?
            .into_pages();

        let limit = options.limit.unwrap_or(usize::MAX);
        let mut keys = Vec::new();
        let mut drained = false;
        while keys.len() < limit {
            let Some(page) = pages.try_next().await.map_err(cosmos_error)? else {
                drained = true;
                break;
            };
            keys.extend(page.into_items().into_iter().map(|row| row.id));
        }

        let cursor = if drained {
            None
        } else {
            Some(
                pages
                    .to_continuation_token()
                    .map_err(cosmos_error)?
                    .as_str()
                    .to_owned(),
            )
        };

        Ok(KvListResult { keys, cursor })
    }
}

/// One row of the listing query.
#[derive(Debug, Deserialize)]
struct KeyRow {
    /// The document id, which is the key.
    id: String,
}

#[cfg(test)]
mod tests {
    use super::{
        counter_value, decode_value, document_layout, encode_value, ttl_seconds, PartitionStrategy,
    };
    use core::time::Duration;
    use skyzen_services::kv::KvError;

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
        assert_eq!(ttl_seconds(Duration::from_secs(3600)).unwrap(), 3600);
    }

    #[test]
    fn ttl_seconds_rejects_values_beyond_i32() {
        assert!(ttl_seconds(Duration::from_secs(u64::from(u32::MAX))).is_err());
    }

    #[test]
    fn a_container_partitioning_on_its_own_field_gets_that_field_filled() {
        let layout = document_layout(&["/partition_key"], PartitionStrategy::SameAsId, true)
            .expect("a single flat path should bind");
        assert_eq!(layout.partition_field.as_deref(), Some("partition_key"));

        let document = layout.document("user:1", b"payload", None);
        assert_eq!(document["id"], "user:1");
        assert_eq!(document["partition_key"], "user:1");
        assert_eq!(document["value"], encode_value(b"payload"));
        assert!(!document.contains_key("ttl"));
    }

    #[test]
    fn a_fixed_partition_puts_every_document_in_one_partition() {
        let layout = document_layout(
            &["/tenant"],
            PartitionStrategy::Fixed("acme".to_owned()),
            true,
        )
        .expect("a fixed strategy should bind");

        let document = layout.document("user:1", b"payload", Some(60));
        assert_eq!(document["tenant"], "acme");
        assert_eq!(document["ttl"], 60);
        assert_eq!(layout.partition_value("user:2"), "acme");
    }

    #[test]
    fn a_container_partitioning_on_the_id_stores_no_redundant_copy_of_the_key() {
        let layout = document_layout(&["/id"], PartitionStrategy::SameAsId, false)
            .expect("an id-partitioned container should bind");
        assert_eq!(layout.partition_field, None);

        let document = layout.document("user:1", b"payload", None);
        assert_eq!(document.len(), 2);
        assert_eq!(document["id"], "user:1");
        assert_eq!(layout.partition_value("user:1"), "user:1");
    }

    #[test]
    fn a_fixed_partition_on_an_id_partitioned_container_is_a_contradiction() {
        let error = document_layout(&["/id"], PartitionStrategy::Fixed("acme".to_owned()), true)
            .expect_err("a fixed partition cannot also be the id");
        assert!(error.to_string().contains("/id"));
    }

    #[test]
    fn a_partition_key_this_backend_cannot_fill_is_refused_at_construction() {
        assert!(matches!(
            document_layout(&["/tenant", "/user"], PartitionStrategy::SameAsId, true),
            Err(KvError::Unsupported(_))
        ));
        assert!(document_layout(&["/a/b"], PartitionStrategy::SameAsId, true).is_err());
        assert!(document_layout(&["tenant"], PartitionStrategy::SameAsId, true).is_err());
        assert!(document_layout(&[] as &[&str], PartitionStrategy::SameAsId, true).is_err());
        // The store's own fields cannot double as the partition key.
        assert!(document_layout(&["/value"], PartitionStrategy::SameAsId, true).is_err());
        assert!(document_layout(&["/ttl"], PartitionStrategy::SameAsId, true).is_err());
    }

    #[test]
    fn a_counter_reads_the_decimal_form_the_redis_backend_stores() {
        assert_eq!(counter_value("hits", b"41").unwrap(), 41);
        assert_eq!(counter_value("hits", b"-1").unwrap(), -1);
        assert!(counter_value("hits", b"not a number").is_err());
        assert!(counter_value("hits", &[0xFF]).is_err());
    }
}
