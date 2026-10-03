//! SQLite-backed Durable Object storage for native runtimes and tests.

use super::{DurableDbBackend, DurableDbError};
use crate::{Db, DbExecResult, DbValue, JsonRow};
use core::future::{ready, Future};

/// A real in-memory `SQLite` implementation of Durable Object SQL storage.
///
/// Each instance owns an isolated single-connection database, so schema and rows remain available
/// for the lifetime of the simulated Durable Object or test fixture.
#[derive(Debug, Clone)]
pub struct SqliteDurableDb {
    db: Db,
}

impl SqliteDurableDb {
    /// Open a new isolated in-memory `SQLite` database.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot initialize the database.
    pub async fn in_memory() -> Result<Self, DurableDbError> {
        Ok(Self {
            db: Db::connect_sqlite_memory()
                .await
                .map_err(DurableDbError::from)?,
        })
    }
}

impl DurableDbBackend for SqliteDurableDb {
    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DurableDbError> {
        let mut statement = self.db.query(query);
        for value in params {
            statement = statement.bind(value.clone());
        }
        let rows = statement
            .fetch_all::<JsonRow<serde_json::Value>>()
            .await
            .map_err(DurableDbError::from)?
            .into_iter()
            .map(JsonRow::into_inner)
            .collect::<Vec<_>>();
        Ok(DbExecResult {
            rows_read: rows.len() as u64,
            rows,
            rows_written: 0,
        })
    }

    async fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> Result<DbExecResult, DurableDbError> {
        let mut statement = self.db.query(query);
        for value in params {
            statement = statement.bind(value.clone());
        }
        statement.execute().await.map_err(DurableDbError::from)
    }

    async fn database_size(&self) -> Result<u64, DurableDbError> {
        let page_count = self
            .db
            .query("PRAGMA page_count")
            .fetch_scalar::<i64>()
            .await
            .map_err(DurableDbError::from)?;
        let page_size = self
            .db
            .query("PRAGMA page_size")
            .fetch_scalar::<i64>()
            .await
            .map_err(DurableDbError::from)?;

        let bytes = page_count
            .checked_mul(page_size)
            .ok_or_else(|| DurableDbError::backend("SQLite database size overflow"))?;
        u64::try_from(bytes)
            .map_err(|_| DurableDbError::backend("SQLite database size was negative"))
    }

    // An in-memory database has no pending persistence I/O: every statement is
    // fully applied when its call returns. The state is volatile for the
    // backend's own lifetime — nothing here claims durability across process
    // or object eviction — so the barrier resolves immediately.
    fn sync(&self) -> impl Future<Output = Result<(), DurableDbError>> + Send {
        let _ = self;
        ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::DurableDb;

    #[tokio::test]
    async fn executes_and_reads_sql() {
        let backend = SqliteDurableDb::in_memory()
            .await
            .expect("in-memory SQLite should initialize");
        let db = DurableDb::new(backend);

        db.query("CREATE TABLE counters (value INTEGER NOT NULL)")
            .execute()
            .await
            .expect("schema should execute");
        db.query("INSERT INTO counters (value) VALUES (?)")
            .bind(7_i64)
            .execute()
            .await
            .expect("insert should execute");
        let value = db
            .query("SELECT value FROM counters")
            .fetch_scalar::<i64>()
            .await
            .expect("select should execute");

        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn sync_resolves_after_writes() {
        let db = DurableDb::new(
            SqliteDurableDb::in_memory()
                .await
                .expect("in-memory SQLite should initialize"),
        );

        db.query("CREATE TABLE pending (value INTEGER NOT NULL)")
            .execute()
            .await
            .expect("schema should execute");
        db.query("INSERT INTO pending (value) VALUES (?)")
            .bind(1_i64)
            .execute()
            .await
            .expect("insert should execute");
        db.sync().await.expect("sync should resolve");
    }
}
