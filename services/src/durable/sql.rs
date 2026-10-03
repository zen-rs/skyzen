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

    /// A result row did not have the shape the type it was fetched into declared.
    #[error("durable database row could not be decoded: {0}")]
    Decode(#[from] crate::sql::RowError),
}

backend_error!(DurableDbError);

service_http_error!(DurableDbError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Serialization(_) => INTERNAL_SERVER_ERROR,
    Self::ParameterCountMismatch { .. } => INTERNAL_SERVER_ERROR,
    Self::SqlParse(_) => INTERNAL_SERVER_ERROR,
    Self::RowNotFound => NOT_FOUND,
    Self::Decode(_) => INTERNAL_SERVER_ERROR,
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
            DbError::Decode(error) => Self::Decode(error),
            error @ (DbError::TransactionsUnsupported
            | DbError::BatchesUnsupported
            | DbError::Conflict
            | DbError::Throttled { .. }
            | DbError::MigrationChanged { .. }
            | DbError::Unauthorized) => Self::backend_with(error.to_string(), error),
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

    /// Resolve once every write the object issued is durable.
    ///
    /// This is the storage layer's own durability barrier, not a statement: on
    /// Cloudflare it is `ctx.storage.sync()`, which settles after the runtime has
    /// persisted all unconfirmed writes — and the runtime holds outgoing
    /// messages that would reveal actor state until they are. A backend whose
    /// writes are already durable when its calls return resolves immediately.
    fn sync(&self) -> impl Future<Output = Result<(), DurableDbError>> + Send;
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
    async fn sync(&'_ self) -> Result<(), DurableDbError>;
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

    /// Wait until every pending write the object issued is durable.
    ///
    /// The durability barrier on the underlying storage — see
    /// [`DurableDbBackend::sync`]. Nothing on the request path calls this; it
    /// exists for code that must bound its measurement or ordering by native
    /// write confirmation rather than by call completion.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime's durability barrier fails.
    pub async fn sync(&self) -> Result<(), DurableDbError> {
        self.0.sync().await
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
    use tokio::sync::{broadcast, mpsc};

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

        // A recorder writes nothing, so there is nothing to flush.
        fn sync(&self) -> impl Future<Output = Result<(), DurableDbError>> + Send {
            let _ = self;
            ready(Ok(()))
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

    /// A backend whose `sync` blocks on a release channel, so a test can prove
    /// the erased wrapper holds the call pending and hands back the barrier's
    /// own outcome rather than resolving early.
    #[derive(Clone)]
    struct GatedBackend {
        entered: mpsc::UnboundedSender<()>,
        release: broadcast::Sender<Result<(), String>>,
    }

    impl GatedBackend {
        fn pair() -> (
            Self,
            mpsc::UnboundedReceiver<()>,
            broadcast::Sender<Result<(), String>>,
        ) {
            let (entered, entered_rx) = mpsc::unbounded_channel();
            let (release, _) = broadcast::channel(1);
            (
                Self {
                    entered,
                    release: release.clone(),
                },
                entered_rx,
                release,
            )
        }
    }

    impl DurableDbBackend for GatedBackend {
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

        // Subscribing before the `entered` signal means the release is always
        // delivered, however the test's send races this call's progress.
        async fn sync(&self) -> Result<(), DurableDbError> {
            let mut rx = self.release.subscribe();
            let _ = self.entered.send(());
            let outcome = rx
                .recv()
                .await
                .map_err(|error| DurableDbError::backend(format!("barrier release: {error}")))?;
            outcome.map_err(DurableDbError::backend)
        }
    }

    #[tokio::test]
    async fn sync_stays_pending_until_the_backend_barrier_resolves() {
        let (backend, mut entered_rx, release_tx) = GatedBackend::pair();
        let db = DurableDb::new(backend);
        let handle = tokio::spawn(async move { db.sync().await });

        entered_rx
            .recv()
            .await
            .expect("the erased sync should reach the backend");
        assert!(
            !handle.is_finished(),
            "sync resolved before the backend barrier released"
        );

        release_tx
            .send(Ok(()))
            .expect("release channel should be open");
        handle
            .await
            .expect("sync task should not panic")
            .expect("sync should resolve once the barrier releases");
    }

    #[tokio::test]
    async fn sync_propagates_the_backend_failure() {
        let (backend, mut entered_rx, release_tx) = GatedBackend::pair();
        let handle = tokio::spawn(async move { DurableDb::new(backend).sync().await });

        entered_rx
            .recv()
            .await
            .expect("the erased sync should reach the backend");
        release_tx
            .send(Err("sentinel barrier failure".to_owned()))
            .expect("release channel should be open");

        let error = handle
            .await
            .expect("sync task should not panic")
            .expect_err("sync should fail when the barrier does");
        assert!(
            error.to_string().contains("sentinel barrier failure"),
            "sync should return the backend's own error, got: {error}"
        );
    }
}
