//! In-memory Durable Object SQL store for testing.

use std::sync::{Arc, RwLock};

use skyzen_services::{durable::sql::{DurableDbBackend, DurableDbError}, DbExecResult, DbValue};

/// In-memory implementation of [`DurableDbBackend`] for testing.
///
/// This is a minimal stub that records executed queries.
/// For full SQL testing, consider using `rusqlite` directly.
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

    /// Get all queries that have been executed.
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
    async fn query(&self, query: &str, _params: &[DbValue]) -> Result<DbExecResult, DurableDbError> {
        self.queries
            .write()
            .map_err(|_| DurableDbError::Backend("lock poisoned".to_owned()))?
            .push(query.to_owned());
        Ok(DbExecResult::default())
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DurableDbError> {
        self.query(query, params).await
    }

    async fn database_size(&self) -> Result<u64, DurableDbError> {
        Ok(0)
    }
}
