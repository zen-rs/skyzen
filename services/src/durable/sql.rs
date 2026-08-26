//! Durable Object database abstraction.

use core::future::Future;
use std::borrow::Cow;

use crate::sql::{DbDialect, DbError, DbExecResult, DbValue, QuerySource, SqlQuery};

/// Errors from Durable Object database operations.
#[derive(Debug, thiserror::Error)]
pub enum DurableDbError {
    /// The underlying storage backend returned an error.
    #[error("durable database error: {message}")]
    Backend {
        /// A human-readable description of what the backend was asked to do.
        message: String,
        /// The backend's own error, when it hands one back.
        #[source]
        source: Option<crate::BoxError>,
    },

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

backend_error!(DurableDbError);

service_http_error!(DurableDbError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Serialization(_) => INTERNAL_SERVER_ERROR,
    Self::ParameterCountMismatch { .. } => INTERNAL_SERVER_ERROR,
    Self::SqlParse(_) => INTERNAL_SERVER_ERROR,
    Self::RowNotFound => NOT_FOUND,
});

impl From<DbError> for DurableDbError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::Backend { message, source } => Self::Backend { message, source },
            DbError::Serialization(error) => Self::Serialization(error),
            DbError::ParameterCountMismatch { expected, actual } => {
                Self::ParameterCountMismatch { expected, actual }
            }
            DbError::SqlParse(message) => Self::SqlParse(message),
            DbError::RowNotFound => Self::RowNotFound,
            error @ (DbError::TransactionsUnsupported
            | DbError::BatchesUnsupported
            | DbError::Conflict
            | DbError::Throttled { .. }) => Self::backend_with(error.to_string(), error),
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
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send;

    /// Execute a statement that does not return rows.
    fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send;

    /// Get the on-disk database size in bytes.
    fn database_size(&self) -> impl Future<Output = Result<u64, DurableDbError>> + Send;
}

service_obj! {
    DurableDbBackendObj: DurableDbBackend;
    async fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> Result<DbExecResult, DurableDbError>;
    async fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> Result<DbExecResult, DurableDbError>;
    async fn database_size(&'_ self) -> Result<u64, DurableDbError>;
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
        SqlQuery::new(self, Cow::Borrowed(sql))
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

/// The query builder returned by [`DurableDb::query`].
///
/// Durable Object SQL is `SQLite`, so it shares [`SqlQuery`] — and with it the `?`-placeholder
/// rewriting and the `LIMIT 1` that `fetch_one`/`fetch_optional` append — with [`Db`](crate::Db).
pub type DurableDbQuery<'a> = SqlQuery<'a, &'a DurableDb>;

impl QuerySource for &DurableDb {
    type Error = DurableDbError;

    fn dialect(&self) -> DbDialect {
        DbDialect::Sqlite
    }

    async fn query(&mut self, sql: &str, params: &[DbValue]) -> Result<DbExecResult, Self::Error> {
        self.0.query(sql, params).await
    }

    async fn execute(
        &mut self,
        sql: &str,
        params: &[DbValue],
    ) -> Result<DbExecResult, Self::Error> {
        self.0.execute(sql, params).await
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{DurableDb, DurableDbBackend, DurableDbError};
    use crate::sql::{DbExecResult, DbValue};
    use core::future::{ready, Future};

    #[derive(Debug, Clone, Default)]
    struct RecordingBackend;

    // Every answer is a constant, so the futures are ready on creation rather than `async` blocks
    // with nothing to await.
    impl DurableDbBackend for RecordingBackend {
        fn query(
            &self,
            _query: &str,
            _params: &[DbValue],
        ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send {
            ready(Ok(DbExecResult::default()))
        }

        fn execute(
            &self,
            _query: &str,
            _params: &[DbValue],
        ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send {
            ready(Ok(DbExecResult::default()))
        }

        fn database_size(&self) -> impl Future<Output = Result<u64, DurableDbError>> + Send {
            ready(Ok(0))
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
