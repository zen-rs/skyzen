//! Portable SQL migrations: an embedded, ordered set of files and the runner that applies them.
//!
//! A migration set is built once, at compile time, by `skyzen::embed_migrations!("migrations")`,
//! and applied by [`Db::migrate`]. The same set runs against sqlx-backed `PostgreSQL`, `MySQL` and
//! `SQLite`, against Cloudflare D1, and against the Aurora Data API, because everything the runner
//! needs is already portable: [`Db::execute_batch`] for atomicity and `?` placeholders for the one
//! parameter it binds.
//!
//! # What the runner guarantees
//!
//! - **Each migration lands exactly once.** Applied versions are recorded in `_skyzen_migrations`,
//!   whose `version` column is the primary key.
//! - **Each migration lands atomically, where the backend can.** A migration's statements and the
//!   row recording it go into one [`Db::execute_batch`], so a failure half way through leaves the
//!   database with neither the schema change nor the record of it. That is a real transaction on
//!   the sqlx backends and D1's `batch()` on D1 — with the standing caveat that **`MySQL` commits
//!   implicitly on DDL**, so a `MySQL` migration that both creates a table and fails afterwards
//!   keeps the table. That is `MySQL`, not Skyzen.
//! - **Applied migrations are immutable.** Every already-applied version has its recorded checksum
//!   compared against the embedded file before anything runs, and a mismatch is
//!   [`DbError::MigrationChanged`] naming the file. Editing a migration that has shipped is the
//!   one mistake whose damage is silent otherwise: production keeps the old schema while the
//!   source says something else.
//! - **`applied_at` comes from the database.** The runner never reads a clock, so a machine with a
//!   skewed clock cannot write a timestamp that misorders the history.
//!
//! # Concurrency
//!
//! Two deployments racing to migrate the same database is handled by the primary key rather than
//! by a lock: both compute the same pending set, both run the same batch, and the loser's batch
//! fails on the version row. The runner then re-reads the table, sees the version present, and
//! reports [`DbError::Conflict`] — not the raw constraint violation, which would read as a bug in
//! the migration.

use crate::sql::{split_statements, BatchStatement, Db, DbDialect, DbError};
use std::borrow::Cow;

/// The `CREATE TABLE IF NOT EXISTS` for the bookkeeping table. Identical on all three dialects.
const CREATE_TABLE: &str = include_str!("migrate/create_table.sql");

/// The bookkeeping read, ordered by version so the runner never has to sort it.
const SELECT_APPLIED: &str = include_str!("migrate/select_applied.sql");

/// One migration file: its version, its name, its SQL and the checksum of the SQL it was built
/// from.
///
/// The text fields are [`Cow`]s rather than `&'static str` because the same type serves two very
/// different producers: `embed_migrations!` bakes borrowed `include_str!` output into the binary,
/// while `skyzen migrate` reads the same directory from disk at deploy time and owns its strings.
/// One type means one runner, and one runner means the CLI cannot drift from what the application
/// would have applied itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    version: u64,
    name: Cow<'static, str>,
    sql: Cow<'static, str>,
    checksum: [u8; 32],
}

impl Migration {
    /// Build a migration from data baked into the binary.
    ///
    /// `const` so `embed_migrations!` can expand into a `static` item; see
    /// [`Migrations::from_static`] for why the expansion has that shape.
    #[must_use]
    pub const fn embedded(
        version: u64,
        name: &'static str,
        sql: &'static str,
        checksum: [u8; 32],
    ) -> Self {
        Self {
            version,
            name: Cow::Borrowed(name),
            sql: Cow::Borrowed(sql),
            checksum,
        }
    }

    /// Build a migration from data read at runtime, as `skyzen migrate` does.
    #[must_use]
    pub const fn owned(version: u64, name: String, sql: String, checksum: [u8; 32]) -> Self {
        Self {
            version,
            name: Cow::Owned(name),
            sql: Cow::Owned(sql),
            checksum,
        }
    }

    /// The number that orders this migration.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// The part of the file name after the version.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The migration's SQL, verbatim.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The SHA-256 of the SQL, as the raw digest.
    #[must_use]
    pub const fn checksum(&self) -> [u8; 32] {
        self.checksum
    }

    /// The SHA-256 as lowercase hex, which is the form recorded in `_skyzen_migrations`.
    #[must_use]
    pub fn checksum_hex(&self) -> String {
        hex::encode(self.checksum)
    }

    /// How this migration is named in an error message: `0001_create_users` reads better than
    /// "version 1".
    fn label(&self) -> String {
        format!("`{}` (version {})", self.name, self.version)
    }
}

/// An ordered set of migrations, applied together by [`Db::migrate`].
#[derive(Debug, Clone, Default)]
pub struct Migrations {
    entries: Cow<'static, [Migration]>,
}

impl Migrations {
    /// Wrap a set baked into the binary.
    ///
    /// This is what `embed_migrations!` calls. The expansion binds the array to a `static` first
    /// and passes a reference to it, rather than passing an array literal: a `Migration` owns a
    /// `Cow`, which has drop glue, and a temporary with drop glue cannot be promoted to `'static`
    /// inside a `static` or `const` initializer.
    #[must_use]
    pub const fn from_static(entries: &'static [Migration]) -> Self {
        Self {
            entries: Cow::Borrowed(entries),
        }
    }

    /// Wrap a set read at runtime.
    #[must_use]
    pub const fn from_owned(entries: Vec<Migration>) -> Self {
        Self {
            entries: Cow::Owned(entries),
        }
    }

    /// The migrations, in the order they are applied.
    #[must_use]
    pub fn as_slice(&self) -> &[Migration] {
        &self.entries
    }

    /// Iterate the migrations in application order.
    pub fn iter(&self) -> core::slice::Iter<'_, Migration> {
        self.entries.iter()
    }

    /// How many migrations the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set holds no migrations at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The migration with `version`, if the set has one.
    #[must_use]
    pub fn get(&self, version: u64) -> Option<&Migration> {
        self.entries
            .iter()
            .find(|migration| migration.version == version)
    }

    /// Check that versions are unique and strictly increasing.
    ///
    /// `embed_migrations!` and `skyzen migrate` both validate this while reading the directory, so
    /// on those paths it can never fire. It runs anyway, at the top of every migrate, because a
    /// [`Migrations`] can also be assembled by hand and an out-of-order set applies its migrations
    /// in an order the file names do not show.
    fn check_order(&self) -> Result<(), DbError> {
        for pair in self.entries.windows(2) {
            if pair[0].version >= pair[1].version {
                return Err(DbError::backend(format!(
                    "migration {} is listed before {}, but migrations must be ordered by strictly \
                     increasing version",
                    pair[0].label(),
                    pair[1].label(),
                )));
            }
        }
        Ok(())
    }
}

impl<'a> IntoIterator for &'a Migrations {
    type Item = &'a Migration;
    type IntoIter = core::slice::Iter<'a, Migration>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// What one [`Db::migrate`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// The versions applied by this call, in the order they ran.
    pub applied: Vec<u64>,
    /// How many of the set's migrations were already applied and therefore skipped.
    pub skipped: usize,
}

impl MigrationReport {
    /// Whether this call changed anything.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

/// One row of `_skyzen_migrations`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct AppliedMigration {
    /// The version that was applied.
    pub version: u64,
    /// The name the migration had when it was applied.
    pub name: String,
    /// The checksum of the SQL that ran, as lowercase hex.
    pub checksum: String,
    /// When it ran, rendered by the database itself.
    pub applied_at: String,
}

/// What [`Db::migration_status`] found.
#[derive(Debug, Clone, Default)]
pub struct MigrationStatus {
    /// Every row `_skyzen_migrations` holds, oldest first — including versions this build does not
    /// embed, which is how a database ahead of the binary shows up.
    pub applied: Vec<AppliedMigration>,
    /// The embedded migrations that have not been applied, in the order they would run.
    pub pending: Vec<Migration>,
}

impl Db {
    /// Apply every migration in `migrations` that this database has not seen.
    ///
    /// The bookkeeping table is created if absent, every already-applied migration has its
    /// checksum verified, and each pending migration is then applied as one atomic batch together
    /// with the row that records it.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::MigrationChanged`] when an applied migration's file has been edited,
    /// [`DbError::Conflict`] when a concurrent runner applied the same version first, and whatever
    /// the backend reports for a migration whose SQL fails.
    pub async fn migrate(&self, migrations: &Migrations) -> Result<MigrationReport, DbError> {
        migrations.check_order()?;
        self.ensure_migrations_table().await?;

        let applied = self.applied_migrations().await?;
        verify_applied_checksums(migrations, &applied)?;

        let mut report = MigrationReport::default();
        for migration in migrations {
            if applied.iter().any(|row| row.version == migration.version) {
                report.skipped += 1;
                continue;
            }
            self.apply_migration(migration).await?;
            report.applied.push(migration.version);
        }

        Ok(report)
    }

    /// Read what has been applied and what is still pending, without changing anything.
    ///
    /// The bookkeeping table is still created if absent: asking a database that has never been
    /// migrated must answer "everything is pending", not fail on a missing table — and probing for
    /// that table by matching the three backends' "no such table" messages would be exactly the
    /// kind of string sniffing this crate avoids elsewhere.
    ///
    /// # Errors
    ///
    /// Returns whatever the backend reports for the bookkeeping statements.
    pub async fn migration_status(
        &self,
        migrations: &Migrations,
    ) -> Result<MigrationStatus, DbError> {
        migrations.check_order()?;
        self.ensure_migrations_table().await?;

        let applied = self.applied_migrations().await?;
        let pending = migrations
            .iter()
            .filter(|migration| !applied.iter().any(|row| row.version == migration.version))
            .cloned()
            .collect();

        Ok(MigrationStatus { applied, pending })
    }

    /// Create `_skyzen_migrations` if it is not there yet.
    async fn ensure_migrations_table(&self) -> Result<(), DbError> {
        self.query(CREATE_TABLE).execute().await?;
        Ok(())
    }

    /// Read every recorded migration, oldest first.
    async fn applied_migrations(&self) -> Result<Vec<AppliedMigration>, DbError> {
        self.query(SELECT_APPLIED).fetch_all().await
    }

    /// Run one migration's statements and its version row as a single atomic batch.
    async fn apply_migration(&self, migration: &Migration) -> Result<(), DbError> {
        let dialect = self.dialect();
        let statements = split_statements(&migration.sql, dialect)?;
        if statements.is_empty() {
            return Err(DbError::backend(format!(
                "migration {} contains no SQL statements",
                migration.label(),
            )));
        }

        let version = i64::try_from(migration.version).map_err(|_| {
            DbError::backend(format!(
                "migration {} has a version that does not fit in the `BIGINT` column it is \
                 recorded in",
                migration.label(),
            ))
        })?;

        let mut batch: Vec<BatchStatement> = statements
            .into_iter()
            .map(BatchStatement::new)
            .collect::<Vec<_>>();
        batch.push(
            BatchStatement::new(insert_version_sql(dialect))
                .bind(version)
                .bind(migration.name.as_ref())
                .bind(migration.checksum_hex()),
        );

        tracing::info!(
            version = migration.version,
            name = %migration.name,
            statements = batch.len() - 1,
            "applying migration",
        );

        match self.execute_batch(batch).await {
            Ok(_) => Ok(()),
            Err(error) => Err(self.explain_apply_failure(migration, error).await),
        }
    }

    /// Turn a failed batch into the error that describes what actually happened.
    ///
    /// The interesting case is a lost race: two runners computed the same pending set, and the
    /// loser's batch failed on the version row's primary key. Re-reading the table separates that
    /// from a migration whose own SQL is broken, without matching on any backend's constraint
    /// violation message.
    async fn explain_apply_failure(&self, migration: &Migration, error: DbError) -> DbError {
        let Ok(applied) = self.applied_migrations().await else {
            // The re-read failed too, so there is nothing to add: report what actually broke.
            return error;
        };
        if applied.iter().any(|row| row.version == migration.version) {
            tracing::error!(
                version = migration.version,
                name = %migration.name,
                "migration lost a race: another runner applied this version first, so this batch \
                 was rolled back and nothing it contained was left behind",
            );
            return DbError::Conflict;
        }
        error
    }
}

/// The version-recording statement for `dialect`.
///
/// Only the timestamp expression differs: `applied_at` is written by the database, so the
/// dialect's own way of rendering `CURRENT_TIMESTAMP` as text is what varies. `PostgreSQL` casts
/// to `TEXT`, `MySQL` has no `TEXT` cast target and uses `CHAR`, and `SQLite`'s
/// `CURRENT_TIMESTAMP` is already text.
const fn insert_version_sql(dialect: DbDialect) -> &'static str {
    match dialect {
        DbDialect::Postgres => include_str!("migrate/insert_version_postgres.sql"),
        DbDialect::MySql => include_str!("migrate/insert_version_mysql.sql"),
        DbDialect::Sqlite => include_str!("migrate/insert_version_sqlite.sql"),
    }
}

/// Compare each applied migration against the file it was applied from.
///
/// A version recorded in the database but absent from `migrations` is reported and allowed: that
/// is what a rollback to an older build looks like, and refusing to start would turn a routine
/// rollback into an outage. A version that is present but *different* is refused, because then the
/// source and the database disagree about what the schema is.
fn verify_applied_checksums(
    migrations: &Migrations,
    applied: &[AppliedMigration],
) -> Result<(), DbError> {
    for row in applied {
        let Some(migration) = migrations.get(row.version) else {
            tracing::warn!(
                version = row.version,
                name = %row.name,
                "the database has a migration this build does not embed; it was left alone",
            );
            continue;
        };

        let embedded = migration.checksum_hex();
        if !embedded.eq_ignore_ascii_case(&row.checksum) {
            return Err(DbError::MigrationChanged {
                version: row.version,
                name: migration.name.clone().into_owned(),
                recorded: row.checksum.clone(),
                embedded,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Migration, Migrations};

    fn migration(version: u64, name: &'static str, sql: &'static str) -> Migration {
        Migration::embedded(version, name, sql, [0u8; 32])
    }

    #[test]
    fn an_out_of_order_set_is_refused_before_anything_runs() {
        let migrations = Migrations::from_owned(vec![
            migration(2, "second", "SELECT 2;"),
            migration(1, "first", "SELECT 1;"),
        ]);
        let error = migrations.check_order().expect_err("out of order");
        assert!(
            error.to_string().contains("second") && error.to_string().contains("first"),
            "{error}"
        );
    }

    #[test]
    fn a_repeated_version_is_refused_even_when_sorted() {
        let migrations = Migrations::from_owned(vec![
            migration(1, "first", "SELECT 1;"),
            migration(1, "again", "SELECT 2;"),
        ]);
        assert!(migrations.check_order().is_err());
    }

    #[test]
    fn an_ordered_set_passes_and_is_addressable_by_version() {
        let migrations = Migrations::from_owned(vec![
            migration(1, "first", "SELECT 1;"),
            migration(7, "seventh", "SELECT 7;"),
        ]);
        migrations.check_order().expect("ordered");
        assert_eq!(migrations.len(), 2);
        assert_eq!(migrations.get(7).expect("present").name(), "seventh");
        assert!(migrations.get(2).is_none());
    }

    #[test]
    fn an_empty_set_is_ordered() {
        Migrations::default()
            .check_order()
            .expect("nothing to order");
        assert!(Migrations::default().is_empty());
    }

    #[test]
    fn the_hex_checksum_is_what_the_bookkeeping_column_stores() {
        let migration = Migration::embedded(1, "init", "SELECT 1;", [0xab; 32]);
        assert_eq!(migration.checksum_hex(), "ab".repeat(32));
        assert_eq!(migration.checksum(), [0xab; 32]);
    }
}

/// The runner against a real database.
///
/// `SQLite` is what makes this a genuine end-to-end test rather than a mock: it is a real SQL
/// engine with real transactions and a real primary key, and it needs no server, so the atomicity
/// and race behaviour being asserted here is the database's, not a stand-in's.
#[cfg(all(
    test,
    not(target_arch = "wasm32"),
    feature = "sqlite",
    any(
        feature = "runtime-tokio-native-tls",
        feature = "runtime-tokio-rustls",
        feature = "runtime-async-std-native-tls",
        feature = "runtime-async-std-rustls"
    )
))]
mod runner_tests {
    use super::{AppliedMigration, Migration, Migrations};
    use crate::sql::{Db, DbError};

    /// Checksums are opaque to the runner — it only ever compares them — so the tests use
    /// distinguishable constants rather than hashing anything.
    const fn migration(version: u64, name: &'static str, sql: &'static str, tag: u8) -> Migration {
        Migration::embedded(version, name, sql, [tag; 32])
    }

    const fn create_users() -> Migration {
        migration(
            1,
            "create_users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL);",
            1,
        )
    }

    const fn seed_users() -> Migration {
        // Two statements *and* a semicolon inside a literal, so the splitting the runner depends
        // on is exercised by the end-to-end path and not only by its unit tests.
        migration(
            2,
            "seed_users",
            "INSERT INTO users (id, email) VALUES (1, 'a@b.c');\n\
             INSERT INTO users (id, email) VALUES (2, 'semi;colon@b.c');",
            2,
        )
    }

    async fn db() -> Db {
        Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect")
    }

    /// The shape `SELECT COUNT(*)` comes back as, declared once rather than inside a test body.
    #[derive(serde::Deserialize)]
    struct Count {
        count: i64,
    }

    async fn applied(db: &Db) -> Vec<AppliedMigration> {
        db.applied_migrations()
            .await
            .expect("bookkeeping table should be readable")
    }

    #[tokio::test]
    async fn a_fresh_database_runs_every_migration_and_records_it() {
        let db = db().await;
        let migrations = Migrations::from_owned(vec![create_users(), seed_users()]);

        let report = db
            .migrate(&migrations)
            .await
            .expect("migrations should run");
        assert_eq!(report.applied, vec![1, 2]);
        assert_eq!(report.skipped, 0);

        let rows = applied(&db).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].version, 1);
        assert_eq!(rows[0].name, "create_users");
        assert_eq!(rows[0].checksum, "01".repeat(32));
        // Written by the database, so the runner never had to read a clock.
        assert!(!rows[0].applied_at.is_empty(), "{:?}", rows[0]);

        let count: Count = db
            .query("SELECT COUNT(*) AS count FROM users")
            .fetch_one()
            .await
            .expect("the seeded rows should be there");
        assert_eq!(count.count, 2);
    }

    #[tokio::test]
    async fn a_second_run_applies_nothing() {
        let db = db().await;
        let migrations = Migrations::from_owned(vec![create_users(), seed_users()]);
        db.migrate(&migrations).await.expect("first run");

        let report = db.migrate(&migrations).await.expect("second run");
        assert!(report.is_empty(), "{report:?}");
        assert_eq!(report.skipped, 2);
        assert_eq!(applied(&db).await.len(), 2);
    }

    #[tokio::test]
    async fn an_edited_migration_is_refused_by_its_checksum() {
        let db = db().await;
        db.migrate(&Migrations::from_owned(vec![create_users()]))
            .await
            .expect("first run");

        // The same version and name, hashing to something else — which is exactly what editing an
        // applied file produces.
        let edited = Migrations::from_owned(vec![
            migration(1, "create_users", "CREATE TABLE users (id INTEGER);", 9),
            seed_users(),
        ]);
        let error = db.migrate(&edited).await.expect_err("edited history");
        let DbError::MigrationChanged {
            version,
            name,
            recorded,
            embedded,
        } = error
        else {
            panic!("expected MigrationChanged, got {error:?}");
        };
        assert_eq!(version, 1);
        assert_eq!(name, "create_users");
        assert_eq!(recorded, "01".repeat(32));
        assert_eq!(embedded, "09".repeat(32));

        // Nothing after the mismatch ran, so the later migration is still pending.
        assert_eq!(applied(&db).await.len(), 1);
    }

    #[tokio::test]
    async fn a_failing_migration_leaves_nothing_behind_and_stays_pending() {
        let db = db().await;
        let broken = migration(
            2,
            "broken",
            "CREATE TABLE audit (id INTEGER);\nCREATE TABLE audit (id INTEGER);",
            2,
        );
        let migrations = Migrations::from_owned(vec![create_users(), broken]);

        let error = db.migrate(&migrations).await.expect_err("the second fails");
        assert!(matches!(error, DbError::Backend { .. }), "{error:?}");

        // The first migration committed; the second rolled back whole, so neither the table it
        // created first nor its bookkeeping row survived.
        let status = db
            .migration_status(&migrations)
            .await
            .expect("status should read");
        assert_eq!(status.applied.len(), 1);
        assert_eq!(status.applied[0].version, 1);
        assert_eq!(status.pending.len(), 1);
        assert_eq!(status.pending[0].version(), 2);

        db.query("SELECT * FROM audit")
            .execute()
            .await
            .expect_err("the rolled-back table must not exist");
    }

    #[tokio::test]
    async fn a_lost_race_is_reported_as_a_conflict() {
        let db = db().await;
        // A migration that can be re-run without failing on its own SQL, so the only thing that
        // can break the second attempt is the version row's primary key.
        let repeatable = migration(
            1,
            "create_users",
            "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY);",
            1,
        );
        let migrations = Migrations::from_owned(vec![repeatable]);
        db.migrate(&migrations).await.expect("the winner commits");

        // Replay the apply as the loser of a race would: it read the bookkeeping table before the
        // winner's row landed, so it still believes this version is pending.
        let error = db
            .apply_migration(&migrations.as_slice()[0])
            .await
            .expect_err("the version row collides");
        assert!(matches!(error, DbError::Conflict), "{error:?}");

        // The loser changed nothing.
        assert_eq!(applied(&db).await.len(), 1);
    }

    #[tokio::test]
    async fn a_migration_holding_no_statements_is_refused() {
        let db = db().await;
        let empty = migration(1, "empty", "-- nothing to do here\n", 1);
        let error = db
            .migrate(&Migrations::from_owned(vec![empty]))
            .await
            .expect_err("an empty migration is a mistake, not a no-op");
        assert!(
            error.to_string().contains("no SQL statements") && error.to_string().contains("empty"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn status_on_an_unmigrated_database_reports_everything_pending() {
        let db = db().await;
        let migrations = Migrations::from_owned(vec![create_users(), seed_users()]);

        let status = db
            .migration_status(&migrations)
            .await
            .expect("status creates the table it reads");
        assert_eq!(status.applied.len(), 0);
        assert_eq!(status.pending.len(), 2);
        assert_eq!(status.pending[0].name(), "create_users");
    }

    #[tokio::test]
    async fn a_version_the_build_does_not_embed_is_left_alone() {
        // A rollback to an older build: the database is ahead. Refusing to start would turn a
        // routine rollback into an outage, so the extra version is reported and ignored.
        let db = db().await;
        let both = Migrations::from_owned(vec![create_users(), seed_users()]);
        db.migrate(&both).await.expect("both apply");

        let older = Migrations::from_owned(vec![create_users()]);
        let report = db
            .migrate(&older)
            .await
            .expect("the older build still runs");
        assert!(report.is_empty(), "{report:?}");
        assert_eq!(report.skipped, 1);

        let status = db.migration_status(&older).await.expect("status");
        assert_eq!(status.applied.len(), 2);
        assert_eq!(status.pending.len(), 0);
    }
}
