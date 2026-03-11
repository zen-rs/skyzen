//! Durable Object SQL storage abstraction.

use core::future::Future;

use http_kit::http_error;
use serde::de::DeserializeOwned;
use skyzen_core::{Extractor, StatusCode};

use crate::maybe_send::{BoxFuture, MaybeSend};

// ── Error type ──

/// Errors from Durable Object SQL operations.
#[derive(Debug, thiserror::Error)]
pub enum DurableSqlError {
    /// The underlying storage backend returned an error.
    #[error("durable sql error: {0}")]
    Backend(String),

    /// Serialization or deserialization failed.
    #[error("durable sql serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// A SQL parameter value.
#[derive(Debug, Clone)]
pub enum SqlValue {
    /// A null value.
    Null,
    /// An integer value.
    Integer(i64),
    /// A floating-point value.
    Real(f64),
    /// A text value.
    Text(String),
    /// A blob value.
    Blob(Vec<u8>),
}

/// Result of a SQL execution.
#[derive(Debug, Clone, Default)]
pub struct SqlResult {
    /// Rows returned by the query, each as a JSON-like map.
    pub rows: Vec<serde_json::Value>,
    /// Number of rows read.
    pub rows_read: u64,
    /// Number of rows written.
    pub rows_written: u64,
}

// ── Layer 1: Public trait ──

/// Durable Object SQL storage (`SQLite`).
///
/// Provides synchronous-style SQL access to the Durable Object's
/// co-located `SQLite` database.
pub trait DurableSqlStore: Send + Sync + Clone + 'static {
    /// Execute a SQL statement with parameters.
    fn exec(
        &self,
        query: &str,
        params: &[SqlValue],
    ) -> impl Future<Output = Result<SqlResult, DurableSqlError>> + MaybeSend;

    /// Get the on-disk database size in bytes.
    fn database_size(&self) -> impl Future<Output = Result<u64, DurableSqlError>> + MaybeSend;
}

// ── Layer 2: Private object-safe trait ──

trait DurableSqlStoreObj: Send + Sync {
    fn exec<'a>(
        &'a self,
        query: &'a str,
        params: &'a [SqlValue],
    ) -> BoxFuture<'a, Result<SqlResult, DurableSqlError>>;
    fn database_size(&self) -> BoxFuture<'_, Result<u64, DurableSqlError>>;
    fn clone_box(&self) -> Box<dyn DurableSqlStoreObj>;
}

// ── Bridge ──

impl<T: DurableSqlStore> DurableSqlStoreObj for T {
    fn exec<'a>(
        &'a self,
        query: &'a str,
        params: &'a [SqlValue],
    ) -> BoxFuture<'a, Result<SqlResult, DurableSqlError>> {
        Box::pin(DurableSqlStore::exec(self, query, params))
    }
    fn database_size(&self) -> BoxFuture<'_, Result<u64, DurableSqlError>> {
        Box::pin(DurableSqlStore::database_size(self))
    }
    fn clone_box(&self) -> Box<dyn DurableSqlStoreObj> {
        Box::new(self.clone())
    }
}

// ── User-facing wrapper ──

/// Type-erased Durable Object SQL extractor.
///
/// Wraps any [`DurableSqlStore`] behind dynamic dispatch and provides
/// convenience methods for typed queries.
pub struct DurableSql(Box<dyn DurableSqlStoreObj>);

impl Clone for DurableSql {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for DurableSql {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableSql").finish_non_exhaustive()
    }
}

impl DurableSql {
    /// Create a new `DurableSql` from any [`DurableSqlStore`] implementation.
    pub fn new(store: impl DurableSqlStore) -> Self {
        Self(Box::new(store))
    }

    /// Execute a SQL statement with parameters.
    ///
    /// # Errors
    ///
    /// Returns [`DurableSqlError`] if the backend operation fails.
    pub async fn exec(
        &self,
        query: &str,
        params: &[SqlValue],
    ) -> Result<SqlResult, DurableSqlError> {
        self.0.exec(query, params).await
    }

    /// Get the on-disk database size in bytes.
    ///
    /// # Errors
    ///
    /// Returns [`DurableSqlError`] if the backend operation fails.
    pub async fn database_size(&self) -> Result<u64, DurableSqlError> {
        self.0.database_size().await
    }

    /// Execute a query and deserialize each row into `T`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableSqlError`] if execution or deserialization fails.
    pub async fn query<T: DeserializeOwned>(
        &self,
        query: &str,
        params: &[SqlValue],
    ) -> Result<Vec<T>, DurableSqlError> {
        let result = self.exec(query, params).await?;
        result
            .rows
            .into_iter()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Execute a query and deserialize the first row into `T`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableSqlError`] if execution or deserialization fails.
    pub async fn query_one<T: DeserializeOwned>(
        &self,
        query: &str,
        params: &[SqlValue],
    ) -> Result<Option<T>, DurableSqlError> {
        let result = self.exec(query, params).await?;
        result
            .rows
            .into_iter()
            .next()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .transpose()
    }
}

http_error!(
    /// The Durable SQL service was not found in request extensions.
    pub DurableSqlNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Durable SQL store not configured. Ensure a DurableSqlStore implementation is injected."
);

impl Extractor for DurableSql {
    type Error = DurableSqlNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(DurableSqlNotConfigured::new())
    }
}
