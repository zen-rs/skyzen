//! In-memory `SQLite` database helper for testing.
//!
//! This utility creates a [`skyzen_services::Db`] backed by in-memory `SQLite`.

use skyzen_services::{Db, DbError};

/// A test helper that owns an in-memory SQLite-backed [`Db`].
///
/// The connection is process-local and fully isolated per instance.
#[derive(Debug, Clone)]
pub struct InMemoryDb {
    db: Db,
}

impl InMemoryDb {
    /// Create a new in-memory `SQLite` database.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` connection cannot be established.
    pub async fn new() -> Result<Self, DbError> {
        let db = Db::connect_sqlite_memory().await?;
        Ok(Self { db })
    }

    /// Create a new in-memory `SQLite` database and run schema SQL.
    ///
    /// This is convenient for tests that need quick table setup.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or schema SQL is invalid.
    pub async fn with_schema(schema_sql: &str) -> Result<Self, DbError> {
        let db = Self::new().await?;

        if !schema_sql.trim().is_empty() {
            db.db.query(schema_sql).execute().await?;
        }

        Ok(db)
    }

    /// Borrow the inner [`Db`] wrapper.
    #[must_use]
    pub const fn db(&self) -> &Db {
        &self.db
    }

    /// Clone the inner [`Db`] wrapper.
    ///
    /// Use this when injecting into request extensions/middleware that require
    /// an owned value.
    #[must_use]
    pub fn clone_db(&self) -> Db {
        self.db.clone()
    }

    /// Consume and return the inner [`Db`] wrapper.
    #[must_use]
    pub fn into_db(self) -> Db {
        self.db
    }
}

impl AsRef<Db> for InMemoryDb {
    fn as_ref(&self) -> &Db {
        self.db()
    }
}
