//! In-memory Durable Object SQL store for testing.

use std::sync::{Arc, RwLock};

use skyzen_services::{
    durable::sql::{DurableDbBackend, DurableDbError},
    DbExecResult, DbValue,
};

/// Recording stub implementation of [`DurableDbBackend`] for testing.
///
/// **This is not a SQL engine.** It executes nothing: every `query` and
/// `execute` call records the SQL string and returns an empty
/// [`DbExecResult`], regardless of the statement. `SELECT`s always produce
/// zero rows, `INSERT`s store nothing, and invalid SQL succeeds.
///
/// Its purpose is asserting *which* statements a handler issued, via
/// [`executed_queries`](Self::executed_queries). For tests that need real SQL
/// semantics, use `InMemoryDb` (in-memory `SQLite`, requires a runtime
/// feature) instead.
#[derive(Debug, Clone, Default)]
pub struct InMemoryDurableDb {
    queries: Arc<RwLock<Vec<String>>>,
}

impl InMemoryDurableDb {
    /// Create a new in-memory Durable SQL store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the SQL strings of all statements issued so far, in order.
    ///
    /// Both `query` and `execute` calls are recorded. Bound parameter values
    /// are not captured.
    ///
    /// # Panics
    ///
    /// Panics if the lock is poisoned.
    #[must_use]
    pub fn executed_queries(&self) -> Vec<String> {
        self.queries.read().expect("lock poisoned").clone()
    }
}

impl DurableDbBackend for InMemoryDurableDb {
    async fn query(
        &self,
        query: &str,
        _params: &[DbValue],
    ) -> Result<DbExecResult, DurableDbError> {
        self.queries
            .write()
            .map_err(|_| DurableDbError::backend("lock poisoned"))?
            .push(query.to_owned());
        Ok(DbExecResult::default())
    }

    async fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> Result<DbExecResult, DurableDbError> {
        self.query(query, params).await
    }

    async fn database_size(&self) -> Result<u64, DurableDbError> {
        Ok(0)
    }
}
