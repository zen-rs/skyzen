//! In-memory `SQLite` database helper for testing.
//!
//! This utility creates a [`skyzen_services::Db`] backed by in-memory `SQLite`.

use skyzen_services::{Db, DbError, MigrationReport, Migrations};

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

    /// Create a new in-memory `SQLite` database and apply `migrations` to it.
    ///
    /// This runs the application's real migration set through the real runner
    /// ([`Db::migrate`]), so a test's schema is the schema a deploy would produce — including the
    /// `_skyzen_migrations` bookkeeping, which is what makes "does this migration apply cleanly?"
    /// a question the test suite answers rather than the first deployment.
    ///
    /// Use [`with_schema`](Self::with_schema) instead when a test wants a table and does not care
    /// about migrations at all.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or a migration does not apply.
    pub async fn with_migrations(migrations: &Migrations) -> Result<Self, DbError> {
        let db = Self::new().await?;
        db.migrate(migrations).await?;
        Ok(db)
    }

    /// Apply `migrations` to an already-created database.
    ///
    /// # Errors
    ///
    /// Returns an error if a migration does not apply.
    pub async fn migrate(&self, migrations: &Migrations) -> Result<MigrationReport, DbError> {
        self.db.migrate(migrations).await
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
