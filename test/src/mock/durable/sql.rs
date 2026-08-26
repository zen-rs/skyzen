//! In-memory Durable Object SQL store for testing.

use core::future::{ready, Future};
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

// Recording a SQL string under a lock is synchronous, so each future is ready on creation rather
// than an `async` block with nothing to await.
impl DurableDbBackend for InMemoryDurableDb {
    fn query(
        &self,
        query: &str,
        _params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send {
        ready(
            self.queries
                .write()
                .map_err(|_| DurableDbError::backend("lock poisoned"))
                .map(|mut queries| {
                    queries.push(query.to_owned());
                    DbExecResult::default()
                }),
        )
    }

    fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + Send {
        self.query(query, params)
    }

    fn database_size(&self) -> impl Future<Output = Result<u64, DurableDbError>> + Send {
        ready(Ok(0))
    }
}
