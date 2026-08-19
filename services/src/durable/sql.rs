//! Durable Object database abstraction.

use core::future::Future;

use serde::de::DeserializeOwned;

use crate::{
    maybe_send::{BoxFuture, MaybeSend},
    sql::{prepare_query_sql, DbDialect, DbError, DbExecResult, DbValue},
};

/// Errors from Durable Object database operations.
#[derive(Debug, thiserror::Error)]
pub enum DurableDbError {
    /// The underlying storage backend returned an error.
    #[error("durable database error: {0}")]
    Backend(String),

    /// Serialization or deserialization failed.
    #[error("durable database serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The number of placeholders and bound values did not match.
    ///
    /// Note that only `?` placeholders are supported; `$1`-style placeholders
    /// are not recognized and are never counted.
    #[error("durable database parameter count mismatch: expected {expected}, got {actual} (only `?` placeholders are counted; `$1`-style placeholders are not supported)")]
    ParameterCountMismatch {
        /// The number of placeholders found in the SQL string.
        expected: usize,
        /// The number of bound values supplied through `bind()`.
        actual: usize,
    },

    /// SQL placeholder rewriting failed.
    #[error("durable database SQL parse error: {0}")]
    SqlParse(String),

    /// A query expected one row but none were returned.
    #[error("durable database row not found")]
    RowNotFound,
}

impl From<DbError> for DurableDbError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::Backend(message) => Self::Backend(message),
            DbError::Serialization(error) => Self::Serialization(error),
            DbError::ParameterCountMismatch { expected, actual } => {
                Self::ParameterCountMismatch { expected, actual }
            }
            DbError::SqlParse(message) => Self::SqlParse(message),
            DbError::RowNotFound => Self::RowNotFound,
            DbError::TransactionsUnsupported => {
                Self::Backend(DbError::TransactionsUnsupported.to_string())
            }
        }
    }
}

/// Durable Object SQL storage.
pub trait DurableDbBackend: Send + Sync + Clone + 'static {
    /// Execute a query that returns rows.
    fn query(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + MaybeSend;

    /// Execute a statement that does not return rows.
    fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + MaybeSend;

    /// Get the on-disk database size in bytes.
    fn database_size(&self) -> impl Future<Output = Result<u64, DurableDbError>> + MaybeSend;
}

trait DurableDbBackendObj: Send + Sync {
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>>;
    fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>>;
    fn database_size(&self) -> BoxFuture<'_, Result<u64, DurableDbError>>;
    fn clone_box(&self) -> Box<dyn DurableDbBackendObj>;
}

impl<T: DurableDbBackend> DurableDbBackendObj for T {
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>> {
        Box::pin(DurableDbBackend::query(self, query, params))
    }

    fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>> {
        Box::pin(DurableDbBackend::execute(self, query, params))
    }

    fn database_size(&self) -> BoxFuture<'_, Result<u64, DurableDbError>> {
        Box::pin(DurableDbBackend::database_size(self))
    }

    fn clone_box(&self) -> Box<dyn DurableDbBackendObj> {
        Box::new(self.clone())
    }
}

/// Type-erased Durable Object database extractor.
pub struct DurableDb(Box<dyn DurableDbBackendObj>);

service_extractor!(
    DurableDb,
    DurableDbNotConfigured,
    "Durable database not configured. Ensure a DurableDbBackend implementation is injected."
);

impl DurableDb {
    /// Create a new `DurableDb` from any [`DurableDbBackend`] implementation.
    pub fn new(store: impl DurableDbBackend) -> Self {
        Self(Box::new(store))
    }

    /// Start building a query against the Durable Object database.
    ///
    /// Use `?` for bind placeholders. `$1`-style placeholders are not
    /// supported and will fail the placeholder/parameter count check.
    #[must_use]
    pub const fn query<'a>(&'a self, sql: &'a str) -> DurableDbQuery<'a> {
        DurableDbQuery {
            db: self,
            sql,
            params: Vec::new(),
        }
    }

    /// Get the on-disk database size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot report its database size.
    pub async fn database_size(&self) -> Result<u64, DurableDbError> {
        self.0.database_size().await
    }
}

/// Query builder for [`DurableDb`].
#[derive(Debug)]
pub struct DurableDbQuery<'a> {
    db: &'a DurableDb,
    sql: &'a str,
    params: Vec<DbValue>,
}

impl DurableDbQuery<'_> {
    /// Bind a parameter value to the query.
    #[must_use]
    pub fn bind<T>(mut self, value: T) -> Self
    where
        T: Into<DbValue>,
    {
        self.params.push(value.into());
        self
    }

    /// Execute a statement that does not return rows.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder validation, backend execution, or
    /// result conversion fails.
    pub async fn execute(self) -> Result<DbExecResult, DurableDbError> {
        let sql = prepare_query_sql(self.sql, self.params.len(), DbDialect::Sqlite)?;
        self.db.0.execute(&sql, &self.params).await
    }

    /// Execute a query and deserialize all rows into `T`.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder validation, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_all<T>(self) -> Result<Vec<T>, DurableDbError>
    where
        T: DeserializeOwned,
    {
        let sql = prepare_query_sql(self.sql, self.params.len(), DbDialect::Sqlite)?;
        let result = self.db.0.query(&sql, &self.params).await?;
        result
            .rows
            .into_iter()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Execute a query and deserialize the first row into `T`, if present.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder validation, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_optional<T>(self) -> Result<Option<T>, DurableDbError>
    where
        T: DeserializeOwned,
    {
        let sql = prepare_query_sql(self.sql, self.params.len(), DbDialect::Sqlite)?;
        let result = self.db.0.query(&sql, &self.params).await?;
        result
            .rows
            .into_iter()
            .next()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .transpose()
    }

    /// Execute a query and deserialize exactly one row into `T`.
    ///
    /// # Errors
    ///
    /// Returns an error if execution fails or the query returns no rows.
    pub async fn fetch_one<T>(self) -> Result<T, DurableDbError>
    where
        T: DeserializeOwned,
    {
        self.fetch_optional()
            .await?
            .ok_or(DurableDbError::RowNotFound)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{DurableDb, DurableDbBackend, DurableDbError};
    use crate::sql::{DbExecResult, DbValue};

    #[derive(Debug, Clone, Default)]
    struct RecordingBackend;

    impl DurableDbBackend for RecordingBackend {
        async fn query(
            &self,
            _query: &str,
            _params: &[DbValue],
        ) -> Result<DbExecResult, DurableDbError> {
            Ok(DbExecResult::default())
        }

        async fn execute(
            &self,
            _query: &str,
            _params: &[DbValue],
        ) -> Result<DbExecResult, DurableDbError> {
            Ok(DbExecResult::default())
        }

        async fn database_size(&self) -> Result<u64, DurableDbError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn execute_validates_placeholder_count() {
        let db = DurableDb::new(RecordingBackend);
        let error = db
            .query("INSERT INTO t (a, b) VALUES (?, ?)")
            .bind(1_i64)
            .execute()
            .await
            .expect_err("mismatched parameter count should fail");
        match error {
            DurableDbError::ParameterCountMismatch { expected, actual } => {
                assert_eq!(expected, 2);
                assert_eq!(actual, 1);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn matching_placeholder_count_executes() {
        let db = DurableDb::new(RecordingBackend);
        db.query("INSERT INTO t (a) VALUES (?)")
            .bind(1_i64)
            .execute()
            .await
            .expect("matching parameter count should execute");
    }
}
