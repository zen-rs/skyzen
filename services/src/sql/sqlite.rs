//! The connection settings [`Db::connect_sqlite_with`](super::Db::connect_sqlite_with) takes,
//! re-exported from sqlx so they are guaranteed to be the version this crate wraps.
//!
//! The backend behind [`Db`](super::Db) lives here too: one read-write connection that every
//! write takes turns on, and, for a database in WAL mode, a pool of read-only connections for the
//! statements that provably only read.

use core::num::NonZeroUsize;

use sqlparser::{
    ast::{Query, SetExpr, Statement},
    dialect::SQLiteDialect,
    parser::Parser,
};
pub use sqlx::sqlite::{
    SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqliteLockingMode, SqliteSynchronous,
};
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};

use super::DbError;

/// A `SQLite` database reached through one writer connection and, in WAL mode, a reader pool.
///
/// `SQLite` admits one writer at a time, and a connection waiting for the write lock does not
/// queue: its busy handler sleeps and polls with backoff, so under sustained contention a writer
/// that started waiting early keeps losing the lock to connections that poll sooner, until the
/// busy timeout fails it with `database is locked`. Holding exactly one read-write connection
/// moves that wait into the pool, which hands its connection out in FIFO order — writers queue
/// instead of racing, and none of them ever meets another connection's write lock.
#[derive(Debug, Clone)]
pub(super) struct SqliteBackend {
    /// The one connection allowed to write.
    writer: SqlitePool,
    /// Read-only connections for provably read-only statements, present only when a reader can
    /// never hold up the writer — WAL journaling in the normal locking mode.
    readers: Option<SqlitePool>,
}

impl SqliteBackend {
    /// Open the writer, find out whether the database admits concurrent readers, and open those.
    pub(super) async fn connect(options: SqliteConnectOptions) -> Result<Self, DbError> {
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            // The writer is the database's only read-write connection, and for an in-memory
            // database it is the database itself: closing it for being idle or old would throw
            // the data away, and in WAL mode it is what keeps the `-wal` and `-shm` files the
            // read-only readers depend on in place.
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options.clone())
            .await?;

        let readers = if admits_concurrent_readers(&writer).await? {
            Some(
                SqlitePoolOptions::new()
                    .max_connections(reader_connections())
                    .connect_with(
                        options
                            .read_only(true)
                            // `PRAGMA optimize` can write `sqlite_stat1`, which a read-only
                            // connection cannot; the writer still runs it if it was asked to.
                            .optimize_on_close(false, None),
                    )
                    .await?,
            )
        } else {
            None
        };

        Ok(Self { writer, readers })
    }

    /// The connection every transaction and batch runs on.
    pub(super) const fn writer(&self) -> &SqlitePool {
        &self.writer
    }

    /// Where the standalone statement `sql` runs: a reader when there are readers and the
    /// statement provably only reads, the writer otherwise.
    pub(super) fn pool_for(&self, sql: &str) -> &SqlitePool {
        match &self.readers {
            Some(readers) if is_read_only(sql) => readers,
            _ => &self.writer,
        }
    }

    /// The reader pool, for tests that check a write sent there is refused.
    #[cfg(test)]
    pub(super) const fn readers(&self) -> Option<&SqlitePool> {
        self.readers.as_ref()
    }
}

/// Whether readers on other connections can run alongside the writer without ever holding it up.
///
/// Only WAL gives that: readers see the last committed snapshot while the writer appends to the
/// log. With a rollback journal a commit has to wait for every reader to finish — through the same
/// polling busy handler — and an in-memory database has no file another connection could read, so
/// both run everything on the writer. An exclusive locking mode keeps every other connection out.
/// The journal mode is read from the open database rather than from the options, because WAL is a
/// property of the file that persists across connections.
async fn admits_concurrent_readers(writer: &SqlitePool) -> Result<bool, DbError> {
    let journal_mode = sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
        .fetch_one(writer)
        .await?;
    let locking_mode = sqlx::query_scalar::<_, String>("PRAGMA locking_mode")
        .fetch_one(writer)
        .await?;
    Ok(journal_mode.eq_ignore_ascii_case("wal") && locking_mode.eq_ignore_ascii_case("normal"))
}

/// How many read-only connections the reader pool may open: one per unit of the host's available
/// parallelism, since each sqlx `SQLite` connection runs its statements on a worker thread of its
/// own.
fn reader_connections() -> u32 {
    let parallelism = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    u32::try_from(parallelism).unwrap_or(u32::MAX)
}

/// Whether `sql` provably only reads: it parses, and every statement in it is a query with no
/// data-modifying part.
///
/// Anything else — a write, a `PRAGMA`, DDL, `EXPLAIN`, an empty string, or SQL the parser does
/// not understand — counts as a possible write and runs on the writer. Routing a read there costs
/// only concurrency, while routing a write to a reader would fail it.
fn is_read_only(sql: &str) -> bool {
    Parser::parse_sql(&SQLiteDialect {}, sql).is_ok_and(|statements| {
        !statements.is_empty()
            && statements.iter().all(|statement| match statement {
                Statement::Query(query) => query_is_read_only(query),
                _ => false,
            })
    })
}

/// Whether a parsed query writes nothing: its `WITH` clauses and its body are all reads.
///
/// `SQLite`'s grammar puts a data-modifying statement only at the top level, optionally after a
/// `WITH` clause — `WITH t AS (…) INSERT …` parses as a query whose body is the insert — so the
/// body and the common table expressions are where one can appear. A nested one is a syntax error
/// in `SQLite` wherever the parser might accept it, and the reader connections are opened
/// read-only, so even a misjudged statement fails there instead of writing.
fn query_is_read_only(query: &Query) -> bool {
    query.with.as_ref().is_none_or(|with| {
        with.cte_tables
            .iter()
            .all(|cte| query_is_read_only(&cte.query))
    }) && set_expr_is_read_only(&query.body)
}

/// Whether a query body writes nothing.
fn set_expr_is_read_only(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(select) => select.into.is_none(),
        SetExpr::Values(_) => true,
        SetExpr::Query(query) => query_is_read_only(query),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr_is_read_only(left) && set_expr_is_read_only(right)
        }
        // `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `TABLE` (which `SQLite` does not have), and
        // whatever the parser adds later: not provably a read.
        _ => false,
    }
}

#[cfg(test)]
mod classification_tests {
    use super::is_read_only;

    #[test]
    fn queries_without_a_data_modifying_part_are_reads() {
        for sql in [
            "SELECT 1",
            "SELECT value FROM entries WHERE id = ?",
            "select count(*) from entries;",
            "SELECT replace(name, 'a', 'b') FROM entries",
            "SELECT * FROM a UNION ALL SELECT * FROM b",
            "WITH recent AS (SELECT * FROM entries ORDER BY id DESC LIMIT 5) SELECT * FROM recent",
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3) \
             SELECT x FROM n",
            "VALUES (1, 2)",
            "SELECT * FROM pragma_table_info('entries')",
            "SELECT 1; SELECT 2",
            "SELECT 'delete from entries' AS text",
        ] {
            assert!(is_read_only(sql), "should read: {sql}");
        }
    }

    #[test]
    fn anything_that_may_write_or_does_not_parse_goes_to_the_writer() {
        for sql in [
            "INSERT INTO entries (value) VALUES (?)",
            "INSERT INTO entries (value) VALUES (1) RETURNING id",
            "REPLACE INTO entries (id, value) VALUES (1, 2)",
            "INSERT OR REPLACE INTO entries (id, value) VALUES (1, 2)",
            "UPDATE entries SET value = 1",
            "DELETE FROM entries RETURNING *",
            "WITH old AS (SELECT id FROM entries) DELETE FROM entries WHERE id IN (SELECT id FROM old)",
            "WITH v AS (SELECT 1 AS x) INSERT INTO entries (value) SELECT x FROM v",
            "SELECT 1; DELETE FROM entries",
            "CREATE TABLE entries (value INTEGER)",
            "CREATE TEMP TABLE scratch (value INTEGER)",
            "DROP TABLE entries",
            "PRAGMA journal_mode",
            "PRAGMA user_version = 3",
            "ATTACH DATABASE 'other.db' AS other",
            "BEGIN",
            "VACUUM",
            "EXPLAIN QUERY PLAN SELECT 1",
            "",
            "-- nothing",
            "SELEKT broken",
        ] {
            assert!(!is_read_only(sql), "should go to the writer: {sql}");
        }
    }
}
