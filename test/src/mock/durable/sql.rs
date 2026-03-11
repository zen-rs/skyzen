//! In-memory Durable Object SQL store for testing.

use std::sync::{Arc, RwLock};

use skyzen_services::durable::sql::{DurableSqlError, DurableSqlStore, SqlResult, SqlValue};

/// In-memory implementation of [`DurableSqlStore`] for testing.
///
/// This is a minimal stub that records executed queries.
/// For full SQL testing, consider using `rusqlite` directly.
#[derive(Debug, Clone, Default)]
pub struct InMemoryDurableSql {
    queries: Arc<RwLock<Vec<String>>>,
}

impl InMemoryDurableSql {
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

impl DurableSqlStore for InMemoryDurableSql {
    async fn exec(&self, query: &str, _params: &[SqlValue]) -> Result<SqlResult, DurableSqlError> {
        self.queries
            .write()
            .map_err(|_| DurableSqlError::Backend("lock poisoned".to_owned()))?
            .push(query.to_owned());
        Ok(SqlResult::default())
    }

    async fn database_size(&self) -> Result<u64, DurableSqlError> {
        Ok(0)
    }
}
