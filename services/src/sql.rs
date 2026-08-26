//! Unified SQL database abstraction.
//!
//! # Placeholders
//!
//! Queries use `?` as the only supported bind placeholder on **every**
//! backend, including `PostgreSQL`:
//!
//! ```ignore
//! let user: User = db
//!     .query("SELECT * FROM users WHERE id = ?")
//!     .bind(7_i64)
//!     .fetch_one()
//!     .await?;
//! ```
//!
//! Skyzen rewrites each `?` into the backend's native form (`$1`, `$2`, … for
//! `PostgreSQL`) before execution. `$1`-style placeholders are **not**
//! recognized: they are passed through verbatim and are not counted, so a
//! query using them fails the placeholder/parameter count check. A `?` inside
//! a string literal or comment is never treated as a placeholder.
//!
//! # Native drivers
//!
//! The `postgres`, `mysql`, and `sqlite` crate features (all enabled by
//! default) control which `sqlx` drivers are compiled in. Driver-backed
//! constructors such as [`Db::connect_sqlite`] only exist on non-wasm targets
//! with the matching feature enabled; on wasm32 targets, use a platform
//! backend (e.g. Cloudflare D1) instead.
//!
//! # Rows travel as JSON, and what that costs
//!
//! [`DbExecResult::rows`] is a `Vec<serde_json::Value>`: every backend converts its driver's
//! native row into JSON, and `fetch_all`/`fetch_one` then deserialize that into the caller's
//! struct. One portable row representation is what lets the same handler run against sqlx and
//! against Cloudflare D1, but JSON cannot represent everything SQL can, and the conversions have
//! consequences worth knowing before they surprise you at runtime:
//!
//! - **`NUMERIC` / `DECIMAL` arrive as strings.** They are exact, and JSON numbers are not, so the
//!   converters render them with `to_string()`. A field typed `f64` will therefore fail to
//!   deserialize; type it as `String`, or as `bigdecimal::BigDecimal`, whose `Deserialize` accepts
//!   the string form.
//! - **Blobs arrive as arrays of integers**, because JSON has no byte string. A `Vec<u8>` field
//!   deserializes fine; a `#[serde(with = "serde_bytes")]` field does not.
//! - **Integers above 2^53** are exact in `serde_json`'s own number type, but lose precision the
//!   moment the value passes through a `f64` — which is what happens if a row is re-serialized by
//!   a JSON implementation without 64-bit integer support.
//! - **`NaN` and infinity have no JSON representation** and render as `null`.
//! - **Timestamps, dates, times and UUIDs arrive as strings** — RFC 3339 for `TIMESTAMPTZ`, the
//!   driver's textual form otherwise. `chrono` and `uuid` both deserialize from exactly those, so
//!   a typed field round-trips; a hand-rolled parser may not.
//!
//! The bind direction has none of these limits: [`DbValue`] carries `Timestamp`, `Uuid`, `Decimal`
//! and `Json` variants that each backend encodes natively (with the one documented exception on
//! [`DbValue::Decimal`]), so a parameter never has to be stringified by the caller.

use std::{borrow::Cow, future::Future};

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use sqlparser::{
    dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
    keywords::Keyword,
    tokenizer::{Location, Token, TokenWithSpan, Tokenizer},
};
use uuid::Uuid;

use crate::BoxFuture;

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
use sqlx::Row;

/// Errors from database operations.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The underlying database backend returned an error.
    #[error("database error: {message}")]
    Backend {
        /// A human-readable description of what the backend was asked to do.
        message: String,
        /// The backend's own error, when it hands one back.
        #[source]
        source: Option<crate::BoxError>,
    },

    /// Serialization or deserialization failed.
    #[error("database serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The number of placeholders and bound values did not match.
    ///
    /// Note that only `?` placeholders are supported; `$1`-style placeholders
    /// are not recognized and are never counted.
    #[error("database parameter count mismatch: expected {expected}, got {actual} (only `?` placeholders are counted; `$1`-style placeholders are not supported)")]
    ParameterCountMismatch {
        /// The number of placeholders found in the SQL string.
        expected: usize,
        /// The number of bound values supplied through `bind()`.
        actual: usize,
    },

    /// SQL placeholder rewriting failed.
    #[error("database SQL parse error: {0}")]
    SqlParse(String),

    /// A query expected one row but none were returned.
    #[error("database row not found")]
    RowNotFound,

    /// The database backend does not support transactions.
    #[error("database transactions are not supported by this backend")]
    TransactionsUnsupported,

    /// The database backend cannot run a set of statements as one atomic batch.
    #[error("database atomic batches are not supported by this backend")]
    BatchesUnsupported,

    /// A write failed because a concurrent transaction changed the same rows.
    #[error("database conflict: a concurrent write changed the same rows")]
    Conflict,

    /// The backend rejected the request because the caller is over its rate limit.
    #[error("database request was throttled by the backend")]
    Throttled {
        /// How long the backend asked the caller to wait, when it says.
        retry_after: Option<core::time::Duration>,
    },
}

backend_error!(DbError);

service_http_error!(DbError {
    Self::Backend { .. } => INTERNAL_SERVER_ERROR,
    Self::Serialization(_) => INTERNAL_SERVER_ERROR,
    Self::ParameterCountMismatch { .. } => INTERNAL_SERVER_ERROR,
    Self::SqlParse(_) => INTERNAL_SERVER_ERROR,
    Self::RowNotFound => NOT_FOUND,
    Self::TransactionsUnsupported => NOT_IMPLEMENTED,
    Self::BatchesUnsupported => NOT_IMPLEMENTED,
    Self::Conflict => CONFLICT,
    Self::Throttled { .. } => TOO_MANY_REQUESTS,
});

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        if matches!(error, sqlx::Error::RowNotFound) {
            return Self::RowNotFound;
        }
        Self::backend_with(error.to_string(), error)
    }
}

/// A SQL parameter value.
///
/// The variants beyond the six SQL storage classes exist so a caller can bind a timestamp, a UUID,
/// an exact decimal or a JSON document without stringifying it by hand and relying on the
/// database's implicit cast. Each backend encodes them in its own native form where it has one —
/// see [`DbValue::Decimal`] for the one place that is not true.
#[derive(Debug, Clone)]
pub enum DbValue {
    /// A null value.
    Null,
    /// A boolean value.
    Boolean(bool),
    /// A signed integer value.
    Integer(i64),
    /// A floating-point value.
    Real(f64),
    /// A text value.
    Text(String),
    /// A blob value.
    Blob(Vec<u8>),
    /// An instant in time, bound as `TIMESTAMPTZ` / `TIMESTAMP` / an ISO-8601 `TEXT` value.
    Timestamp(DateTime<Utc>),
    /// A UUID, bound as `UUID` on `PostgreSQL` and as bytes elsewhere.
    Uuid(Uuid),
    /// An exact decimal, bound as `NUMERIC` / `DECIMAL`.
    ///
    /// `SQLite` has no decimal type and sqlx has no `SQLite` encoder for one, so on that backend
    /// the value is bound as its decimal `TEXT` rendering. Comparisons against it are then string
    /// comparisons, which is a real difference in behaviour, not just in storage.
    Decimal(BigDecimal),
    /// A JSON document, bound as `JSON` / `JSONB` where the backend has one.
    Json(serde_json::Value),
}

impl From<bool> for DbValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i8> for DbValue {
    fn from(value: i8) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<i16> for DbValue {
    fn from(value: i16) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<i32> for DbValue {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<i64> for DbValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<u8> for DbValue {
    fn from(value: u8) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<u16> for DbValue {
    fn from(value: u16) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<u32> for DbValue {
    fn from(value: u32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<f32> for DbValue {
    fn from(value: f32) -> Self {
        Self::Real(f64::from(value))
    }
}

impl From<f64> for DbValue {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}

impl From<String> for DbValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for DbValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<Vec<u8>> for DbValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value)
    }
}

impl From<&[u8]> for DbValue {
    fn from(value: &[u8]) -> Self {
        Self::Blob(value.to_vec())
    }
}

impl From<DateTime<Utc>> for DbValue {
    fn from(value: DateTime<Utc>) -> Self {
        Self::Timestamp(value)
    }
}

impl From<Uuid> for DbValue {
    fn from(value: Uuid) -> Self {
        Self::Uuid(value)
    }
}

impl From<BigDecimal> for DbValue {
    fn from(value: BigDecimal) -> Self {
        Self::Decimal(value)
    }
}

impl From<serde_json::Value> for DbValue {
    fn from(value: serde_json::Value) -> Self {
        Self::Json(value)
    }
}

impl<T> From<Option<T>> for DbValue
where
    T: Into<Self>,
{
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// Result of a SQL execution.
#[derive(Debug, Clone, Default)]
pub struct DbExecResult {
    /// Rows returned by the query, each as a JSON-like map.
    pub rows: Vec<serde_json::Value>,
    /// Number of rows read.
    pub rows_read: u64,
    /// Number of rows written.
    pub rows_written: u64,
}

/// One statement in an atomic batch.
///
/// Built the same way a [`Db::query`] is — `?` placeholders in the SQL, one bound value per
/// placeholder — but held rather than executed, so a whole set can be handed to
/// [`Db::execute_batch`] at once.
#[derive(Debug, Clone)]
pub struct BatchStatement {
    /// The SQL to run, with `?` for every bind placeholder on every dialect.
    pub sql: String,
    /// The values bound to those placeholders, in order.
    pub params: Vec<DbValue>,
}

impl BatchStatement {
    /// Start a statement with no bound values yet.
    #[must_use]
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    /// Bind the next parameter value.
    #[must_use]
    pub fn bind(mut self, value: impl Into<DbValue>) -> Self {
        self.params.push(value.into());
        self
    }
}

/// SQL dialect expected by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbDialect {
    /// `PostgreSQL` syntax and placeholder rules.
    Postgres,
    /// `MySQL` syntax and placeholder rules.
    MySql,
    /// `SQLite` syntax and placeholder rules.
    Sqlite,
}

/// A unified SQL database backend.
pub trait DbBackend: Send + Sync + Clone + 'static {
    /// Which SQL dialect this backend expects.
    fn dialect(&self) -> DbDialect;

    /// Execute a statement that returns rows.
    fn query(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + Send;

    /// Execute a statement that does not return rows.
    fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + Send;

    /// Begin a database transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot create a transaction.
    fn begin(&self) -> impl Future<Output = Result<DbTransaction, DbError>> + Send {
        async { Err(DbError::TransactionsUnsupported) }
    }

    /// Run `statements` as one atomic unit, returning one result per statement in order.
    ///
    /// Either every statement lands or none does. The SQL arrives already rewritten for this
    /// backend's dialect, exactly as it does for [`query`](DbBackend::query) and
    /// [`execute`](DbBackend::execute) — [`Db::execute_batch`] does the rewriting.
    fn execute_batch(
        &self,
        statements: Vec<BatchStatement>,
    ) -> impl Future<Output = Result<Vec<DbExecResult>, DbError>> + Send {
        let _ = statements;
        async { Err(DbError::BatchesUnsupported) }
    }
}

/// A mutable database transaction backend.
pub trait DbTransactionBackend: Send + 'static {
    /// Which SQL dialect this transaction expects.
    fn dialect(&self) -> DbDialect;

    /// Execute a statement that returns rows inside this transaction.
    fn query(
        &mut self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + Send;

    /// Execute a statement that does not return rows inside this transaction.
    fn execute(
        &mut self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + Send;

    /// Commit this transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot commit the transaction.
    fn commit(self) -> impl Future<Output = Result<(), DbError>> + Send
    where
        Self: Sized;

    /// Roll back this transaction.
    fn rollback(self) -> impl Future<Output = Result<(), DbError>> + Send
    where
        Self: Sized;
}

service_obj! {
    DbBackendObj: DbBackend;
    fn dialect(&self) -> DbDialect;
    async fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> Result<DbExecResult, DbError>;
    async fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> Result<DbExecResult, DbError>;
    async fn begin(&'_ self) -> Result<DbTransaction, DbError>;
    async fn execute_batch(
        &'_ self,
        statements: Vec<BatchStatement>,
    ) -> Result<Vec<DbExecResult>, DbError>;
}

/// The object-safe mirror of [`DbTransactionBackend`].
///
/// Transactions are the one service that `service_obj!` cannot generate: their methods take
/// `&mut self`, `commit`/`rollback` consume the backend through `self: Box<Self>`, and there is no
/// `clone_box` because a transaction is not clonable.
trait DbTransactionBackendObj: Send {
    fn dialect(&self) -> DbDialect;
    fn query<'a>(
        &'a mut self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>>;
    fn execute<'a>(
        &'a mut self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>>;
    fn commit(self: Box<Self>) -> BoxFuture<'static, Result<(), DbError>>;
    fn rollback(self: Box<Self>) -> BoxFuture<'static, Result<(), DbError>>;
}

impl<T: DbTransactionBackend> DbTransactionBackendObj for T {
    fn dialect(&self) -> DbDialect {
        DbTransactionBackend::dialect(self)
    }

    fn query<'a>(
        &'a mut self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>> {
        Box::pin(DbTransactionBackend::query(self, query, params))
    }

    fn execute<'a>(
        &'a mut self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>> {
        Box::pin(DbTransactionBackend::execute(self, query, params))
    }

    fn commit(self: Box<Self>) -> BoxFuture<'static, Result<(), DbError>> {
        Box::pin(async move { DbTransactionBackend::commit(*self).await })
    }

    fn rollback(self: Box<Self>) -> BoxFuture<'static, Result<(), DbError>> {
        Box::pin(async move { DbTransactionBackend::rollback(*self).await })
    }
}

/// A type-erased SQL database extractor.
pub struct Db(Box<dyn DbBackendObj>);

service_extractor!(
    Db,
    DbNotConfigured,
    "Database not configured. Ensure a DbBackend implementation is injected."
);

impl Db {
    /// Create a new `Db` from any [`DbBackend`] implementation.
    pub fn new(store: impl DbBackend) -> Self {
        Self(Box::new(store))
    }

    /// Start building a SQL query.
    ///
    /// Use `?` for every bind placeholder, on every backend — including
    /// `PostgreSQL`, where Skyzen rewrites `?` to `$1`, `$2`, … before
    /// execution. `$1`-style placeholders are **not** supported and will fail
    /// the placeholder/parameter count check.
    #[must_use]
    pub const fn query<'a>(&'a self, sql: &'a str) -> DbQuery<'a> {
        SqlQuery::new(self, Cow::Borrowed(sql))
    }

    /// Begin a database transaction.
    ///
    /// # Portability
    ///
    /// An interactive transaction — `BEGIN`, then application logic between statements, then
    /// `COMMIT` — needs a connection the caller holds open, which the serverless SQL backends do
    /// not offer. Cloudflare D1 in particular runs every statement in auto-commit and returns
    /// [`DbError::BatchesUnsupported`]'s neighbour, [`DbError::TransactionsUnsupported`], here.
    /// [`execute_batch`](Self::execute_batch) is the portable atomic path: it is a real
    /// transaction on the native sqlx backends and D1's own `batch()` on D1.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot create a transaction.
    pub async fn begin(&self) -> Result<DbTransaction, DbError> {
        self.0.begin().await
    }

    /// Run `statements` as one atomic unit, returning one result per statement in order.
    ///
    /// Either every statement lands or none does. This is the atomicity primitive that works on
    /// every backend, including the ones that cannot hold a connection open for
    /// [`begin`](Self::begin).
    ///
    /// Each statement's SQL is rewritten for the backend's dialect and checked against its bound
    /// parameter count first, exactly as [`query`](Self::query) does — so `?` placeholders are
    /// correct everywhere, `PostgreSQL` included.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::BatchesUnsupported`] when the backend has no atomic batch, a
    /// placeholder-rewriting error for a malformed statement, or whatever the backend reports for
    /// the statement that failed.
    pub async fn execute_batch(
        &self,
        statements: Vec<BatchStatement>,
    ) -> Result<Vec<DbExecResult>, DbError> {
        let dialect = self.0.dialect();
        let statements = statements
            .into_iter()
            .map(|statement| {
                Ok(BatchStatement {
                    sql: prepare_query_sql(&statement.sql, statement.params.len(), dialect)?,
                    params: statement.params,
                })
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        self.0.execute_batch(statements).await
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "postgres"))]
    /// Connect to a `PostgreSQL` database using `sqlx`.
    ///
    /// # Errors
    ///
    /// Returns an error if `sqlx` cannot establish the connection.
    pub async fn connect_postgres(url: &str) -> Result<Self, DbError> {
        Ok(Self::new(NativeDbBackend::Postgres(
            sqlx::postgres::PgPoolOptions::new().connect(url).await?,
        )))
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "mysql"))]
    /// Connect to a `MySQL` database using `sqlx`.
    ///
    /// # Errors
    ///
    /// Returns an error if `sqlx` cannot establish the connection.
    pub async fn connect_mysql(url: &str) -> Result<Self, DbError> {
        Ok(Self::new(NativeDbBackend::MySql(
            sqlx::mysql::MySqlPoolOptions::new().connect(url).await?,
        )))
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
    /// Connect to a `SQLite` database using `sqlx`.
    ///
    /// # Errors
    ///
    /// Returns an error if `sqlx` cannot establish the connection.
    pub async fn connect_sqlite(url: &str) -> Result<Self, DbError> {
        Ok(Self::new(NativeDbBackend::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new().connect(url).await?,
        )))
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
    /// Connect to an in-memory `SQLite` database using a single connection.
    ///
    /// # Errors
    ///
    /// Returns an error if `sqlx` cannot initialize the in-memory database.
    pub async fn connect_sqlite_memory() -> Result<Self, DbError> {
        Ok(Self::new(NativeDbBackend::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await?,
        )))
    }
}

/// A type-erased database transaction.
pub struct DbTransaction(Box<dyn DbTransactionBackendObj>);

impl std::fmt::Debug for DbTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbTransaction").finish_non_exhaustive()
    }
}

impl DbTransaction {
    /// Wrap a concrete transaction backend.
    pub fn new(tx: impl DbTransactionBackend) -> Self {
        Self(Box::new(tx))
    }

    /// Start building a SQL query within this transaction.
    pub const fn query<'a>(&'a mut self, sql: &'a str) -> DbTransactionQuery<'a> {
        SqlQuery::new(self, Cow::Borrowed(sql))
    }

    /// Commit this transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot commit the transaction.
    pub async fn commit(self) -> Result<(), DbError> {
        self.0.commit().await
    }

    /// Roll this transaction back.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot roll back the transaction.
    pub async fn rollback(self) -> Result<(), DbError> {
        self.0.rollback().await
    }
}

/// The execution surface the query builder is written against.
///
/// [`Db`], [`DbTransaction`] and [`DurableDb`](crate::durable::DurableDb) differ only in how they
/// run a prepared statement and which error they report, so [`SqlQuery`] is implemented once over
/// this trait instead of three times over the three of them. It is plumbing, not an extension
/// point: implementing it outside this crate buys nothing, because the builder is only reachable
/// from the three `query()` methods.
pub trait QuerySource: Send {
    /// The error this source reports.
    ///
    /// The `From` bounds are what let the shared builder raise a placeholder-rewriting failure or
    /// a row-decoding failure without knowing which of the three errors it is producing.
    type Error: From<DbError> + From<serde_json::Error> + Send;

    /// Which SQL dialect statements run through this source are written in.
    fn dialect(&self) -> DbDialect;

    /// Execute a statement that returns rows.
    fn query(
        &mut self,
        sql: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, Self::Error>> + Send;

    /// Execute a statement that does not return rows.
    fn execute(
        &mut self,
        sql: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, Self::Error>> + Send;
}

impl QuerySource for &Db {
    type Error = DbError;

    fn dialect(&self) -> DbDialect {
        self.0.dialect()
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

impl QuerySource for &mut DbTransaction {
    type Error = DbError;

    fn dialect(&self) -> DbDialect {
        self.0.dialect()
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

/// A SQL query being built.
///
/// One builder serves [`Db`], [`DbTransaction`] and
/// [`DurableDb`](crate::durable::DurableDb) — see the [`DbQuery`], [`DbTransactionQuery`] and
/// [`DurableDbQuery`](crate::durable::DurableDbQuery) aliases, which are what the three `query()`
/// methods hand back.
#[derive(Debug)]
pub struct SqlQuery<'a, S> {
    source: S,
    sql: Cow<'a, str>,
    params: Vec<DbValue>,
}

/// The query builder returned by [`Db::query`].
pub type DbQuery<'a> = SqlQuery<'a, &'a Db>;

/// The query builder returned by [`DbTransaction::query`].
pub type DbTransactionQuery<'a> = SqlQuery<'a, &'a mut DbTransaction>;

impl<'a, S> SqlQuery<'a, S> {
    /// Start a query against `source`.
    pub(crate) const fn new(source: S, sql: Cow<'a, str>) -> Self {
        Self {
            source,
            sql,
            params: Vec::new(),
        }
    }

    /// Bind a parameter value to the query.
    #[must_use]
    pub fn bind<T>(mut self, value: T) -> Self
    where
        T: Into<DbValue>,
    {
        self.params.push(value.into());
        self
    }
}

impl<S: QuerySource> SqlQuery<'_, S> {
    /// Execute a statement that does not return rows.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or result
    /// conversion fails.
    pub async fn execute(mut self) -> Result<DbExecResult, S::Error> {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.source.dialect())?;
        self.source.execute(&sql, &self.params).await
    }

    /// Execute a query and deserialize all rows into `T`.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_all<T>(mut self) -> Result<Vec<T>, S::Error>
    where
        T: DeserializeOwned,
    {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.source.dialect())?;
        let result = self.source.query(&sql, &self.params).await?;
        result
            .rows
            .into_iter()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Execute a query and deserialize the first row into `T`, if present.
    ///
    /// A `LIMIT 1` is appended when the statement is a `SELECT` that does not already bound its
    /// own result set, so the backend stops after the row that is actually used instead of
    /// transferring and converting every match. See [`append_single_row_limit`] for exactly when
    /// that applies.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_optional<T>(mut self) -> Result<Option<T>, S::Error>
    where
        T: DeserializeOwned,
    {
        let dialect = self.source.dialect();
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), dialect)?;
        let sql = append_single_row_limit(&sql, dialect)?;
        let result = self.source.query(&sql, &self.params).await?;
        result
            .rows
            .into_iter()
            .next()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .transpose()
    }

    /// Execute a query and deserialize exactly one row into `T`.
    ///
    /// # Errors
    ///
    /// Returns an error if execution fails or the query returns no rows.
    pub async fn fetch_one<T>(self) -> Result<T, S::Error>
    where
        T: DeserializeOwned,
    {
        self.fetch_optional()
            .await?
            .ok_or_else(|| DbError::RowNotFound.into())
    }
}

pub(crate) fn prepare_query_sql(
    query: &str,
    actual_params: usize,
    dialect: DbDialect,
) -> Result<String, DbError> {
    let tokens = tokenize_sql(query, dialect)?;
    let mut expected_params = 0usize;
    let mut cursor = 0usize;
    let mut rendered = String::with_capacity(query.len() + actual_params.saturating_mul(2));
    let mut mapper = LocationMapper::new(query);

    for token in tokens {
        let start = mapper.byte_index(token.span.start);
        rendered.push_str(&query[cursor..start]);

        if is_bind_placeholder(&token, dialect) {
            expected_params += 1;
            match dialect {
                DbDialect::Postgres => {
                    rendered.push('$');
                    rendered.push_str(&expected_params.to_string());
                }
                DbDialect::MySql | DbDialect::Sqlite => rendered.push('?'),
            }
            // A bind placeholder is always the single byte `?`. Its reported
            // span end cannot be trusted: sqlparser's `Question` token
            // swallows one character of lookahead, so the span may extend
            // past the following whitespace and using it would delete that
            // character from the rendered query.
            cursor = start + 1;
        } else {
            let end = mapper.byte_index(token.span.end);
            rendered.push_str(&query[start..end]);
            cursor = end;
        }
    }

    rendered.push_str(&query[cursor..]);

    if expected_params != actual_params {
        return Err(DbError::ParameterCountMismatch {
            expected: expected_params,
            actual: actual_params,
        });
    }

    Ok(rendered)
}

/// Append `LIMIT 1` to a statement whose caller only reads the first row.
///
/// `fetch_one` and `fetch_optional` otherwise pay for every matching row: the backend transfers
/// them all and the converter turns each into a `serde_json::Value` before all but one is dropped.
/// `LIMIT 1` is understood by all three dialects, so no dialect-specific rendering is needed —
/// only the decision of whether appending it is safe, which is deliberately conservative:
///
/// - the statement must start with `SELECT` or `WITH`, so `INSERT ... RETURNING`, `CALL` and DDL
///   are left alone;
/// - it must contain no `LIMIT`, `FETCH`, `TOP` or `OFFSET` of its own, since the caller's own
///   bound wins and a second `LIMIT` is a syntax error;
/// - it must contain no `FOR` (`FOR UPDATE`, `FOR SHARE`) or `INTO`, because `LIMIT` has to
///   precede those clauses rather than follow them;
/// - it must contain no `;` except a trailing one, so a multi-statement string is never rewritten.
///
/// Anything that fails a check is returned unchanged, which costs the optimization and nothing
/// else. The clause goes on its own line so a trailing `--` comment cannot swallow it.
fn append_single_row_limit(sql: &str, dialect: DbDialect) -> Result<Cow<'_, str>, DbError> {
    /// Clauses that either already bound the result set or must come after `LIMIT`.
    const BLOCKING: &[Keyword] = &[
        Keyword::LIMIT,
        Keyword::FETCH,
        Keyword::TOP,
        Keyword::OFFSET,
        Keyword::FOR,
        Keyword::INTO,
    ];

    let tokens = tokenize_sql(sql, dialect)?;

    let starts_a_query = tokens
        .iter()
        .find_map(|token| match &token.token {
            Token::Word(word) => Some(word.keyword),
            _ => None,
        })
        .is_some_and(|keyword| matches!(keyword, Keyword::SELECT | Keyword::WITH));
    if !starts_a_query {
        return Ok(Cow::Borrowed(sql));
    }

    let last_meaningful = tokens
        .iter()
        .rposition(|token| !matches!(token.token, Token::Whitespace(_)));
    let has_inner_semicolon = tokens.iter().enumerate().any(|(index, token)| {
        matches!(token.token, Token::SemiColon) && Some(index) != last_meaningful
    });
    if has_inner_semicolon {
        return Ok(Cow::Borrowed(sql));
    }

    let bounded_already = tokens.iter().any(|token| match &token.token {
        Token::Word(word) => BLOCKING.contains(&word.keyword),
        _ => false,
    });
    if bounded_already {
        return Ok(Cow::Borrowed(sql));
    }

    let trimmed = sql.trim_end();
    let body = trimmed.strip_suffix(';').unwrap_or(trimmed);
    let mut rendered = String::with_capacity(body.len() + LIMIT_ONE_CLAUSE.len());
    rendered.push_str(body);
    rendered.push_str(LIMIT_ONE_CLAUSE);
    Ok(Cow::Owned(rendered))
}

/// The clause [`append_single_row_limit`] adds, newline-prefixed so a trailing `--` comment in the
/// caller's SQL cannot comment it out.
const LIMIT_ONE_CLAUSE: &str = "\nLIMIT 1";

fn tokenize_sql(query: &str, dialect: DbDialect) -> Result<Vec<TokenWithSpan>, DbError> {
    match dialect {
        DbDialect::Postgres => tokenize_with_dialect(query, &PostgreSqlDialect {}),
        DbDialect::MySql => tokenize_with_dialect(query, &MySqlDialect {}),
        DbDialect::Sqlite => tokenize_with_dialect(query, &SQLiteDialect {}),
    }
}

fn tokenize_with_dialect(
    query: &str,
    dialect: &dyn Dialect,
) -> Result<Vec<TokenWithSpan>, DbError> {
    let mut tokenizer = Tokenizer::new(dialect, query);
    tokenizer
        .tokenize_with_location()
        .map_err(|error| DbError::SqlParse(error.to_string()))
}

fn is_bind_placeholder(token: &TokenWithSpan, dialect: DbDialect) -> bool {
    match (&token.token, dialect) {
        (Token::Placeholder(value), _) if value == "?" => true,
        // PostgreSQL reserves `?` for operators in the tokenizer, so we intentionally treat bare
        // `?` as the unified bind placeholder in Skyzen's query API.
        (Token::Question, DbDialect::Postgres) => true,
        _ => false,
    }
}

/// Maps 1-based line/column [`Location`]s to byte indices in a single forward
/// pass over the query.
///
/// Token spans arrive in source order, so the mapper only ever advances; the
/// total cost of mapping every token location is `O(n)` in the query length
/// (a per-token rescan from the start would be `O(n²)`).
struct LocationMapper<'a> {
    chars: core::iter::Peekable<core::str::CharIndices<'a>>,
    len: usize,
    line: u64,
    column: u64,
}

impl<'a> LocationMapper<'a> {
    fn new(query: &'a str) -> Self {
        Self {
            chars: query.char_indices().peekable(),
            len: query.len(),
            line: 1,
            column: 1,
        }
    }

    /// Byte index of `target`, saturating to the end of the query.
    ///
    /// Targets must be requested in non-decreasing source order.
    fn byte_index(&mut self, target: Location) -> usize {
        if target.line == 0 && target.column == 0 {
            return 0;
        }

        while let Some(&(index, ch)) = self.chars.peek() {
            if (self.line, self.column) >= (target.line, target.column) {
                return index;
            }

            self.chars.next();
            if ch == '\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }

        self.len
    }
}

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
#[derive(Debug, Clone)]
enum NativeDbBackend {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    #[cfg(feature = "mysql")]
    MySql(sqlx::MySqlPool),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
}

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
enum NativeDbTransaction {
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    #[cfg(feature = "mysql")]
    MySql(sqlx::Transaction<'static, sqlx::MySql>),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
}

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
impl DbBackend for NativeDbBackend {
    fn dialect(&self) -> DbDialect {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => DbDialect::Postgres,
            #[cfg(feature = "mysql")]
            Self::MySql(_) => DbDialect::MySql,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => DbDialect::Sqlite,
        }
    }

    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(pool) => query_postgres(pool, query, params).await,
            #[cfg(feature = "mysql")]
            Self::MySql(pool) => query_mysql(pool, query, params).await,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(pool) => query_sqlite(pool, query, params).await,
        }
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(pool) => execute_postgres(pool, query, params).await,
            #[cfg(feature = "mysql")]
            Self::MySql(pool) => execute_mysql(pool, query, params).await,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(pool) => execute_sqlite(pool, query, params).await,
        }
    }

    async fn begin(&self) -> Result<DbTransaction, DbError> {
        let tx = match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(pool) => NativeDbTransaction::Postgres(pool.begin().await?),
            #[cfg(feature = "mysql")]
            Self::MySql(pool) => NativeDbTransaction::MySql(pool.begin().await?),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(pool) => NativeDbTransaction::Sqlite(pool.begin().await?),
        };
        Ok(DbTransaction::new(tx))
    }

    /// Run the batch inside one real sqlx transaction.
    ///
    /// Every statement goes through the row-returning path so a `SELECT` inside a batch hands its
    /// rows back, matching D1's `batch()`. The first failure rolls the whole thing back; the
    /// commit only happens once all of them have succeeded.
    async fn execute_batch(
        &self,
        statements: Vec<BatchStatement>,
    ) -> Result<Vec<DbExecResult>, DbError> {
        let mut transaction = DbBackend::begin(self).await?;
        let mut results = Vec::with_capacity(statements.len());

        for statement in &statements {
            match transaction.0.query(&statement.sql, &statement.params).await {
                Ok(result) => results.push(result),
                Err(error) => {
                    // The batch is all-or-nothing, so a failure here discards the statements that
                    // already ran. A rollback that itself fails is reported instead, because then
                    // the caller genuinely does not know what landed.
                    transaction.rollback().await?;
                    return Err(error);
                }
            }
        }

        transaction.commit().await?;
        Ok(results)
    }
}

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
impl DbTransactionBackend for NativeDbTransaction {
    fn dialect(&self) -> DbDialect {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => DbDialect::Postgres,
            #[cfg(feature = "mysql")]
            Self::MySql(_) => DbDialect::MySql,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(_) => DbDialect::Sqlite,
        }
    }

    async fn query(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(tx) => query_postgres_with(&mut **tx, query, params).await,
            #[cfg(feature = "mysql")]
            Self::MySql(tx) => query_mysql_with(&mut **tx, query, params).await,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(tx) => query_sqlite_with(&mut **tx, query, params).await,
        }
    }

    async fn execute(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(tx) => execute_postgres_with(&mut **tx, query, params).await,
            #[cfg(feature = "mysql")]
            Self::MySql(tx) => execute_mysql_with(&mut **tx, query, params).await,
            #[cfg(feature = "sqlite")]
            Self::Sqlite(tx) => execute_sqlite_with(&mut **tx, query, params).await,
        }
    }

    async fn commit(self) -> Result<(), DbError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(tx) => tx.commit().await.map_err(Into::into),
            #[cfg(feature = "mysql")]
            Self::MySql(tx) => tx.commit().await.map_err(Into::into),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(tx) => tx.commit().await.map_err(Into::into),
        }
    }

    async fn rollback(self) -> Result<(), DbError> {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(tx) => tx.rollback().await.map_err(Into::into),
            #[cfg(feature = "mysql")]
            Self::MySql(tx) => tx.rollback().await.map_err(Into::into),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(tx) => tx.rollback().await.map_err(Into::into),
        }
    }
}

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
/// Bind every [`DbValue`] onto a sqlx query.
///
/// The first argument names how [`DbValue::Decimal`] is encoded: `numeric` for the backends sqlx
/// can encode a `BigDecimal` for (`PostgreSQL`, `MySQL`) and `decimal_text` for `SQLite`, which has
/// no decimal type and therefore no encoder.
macro_rules! bind_query_values {
    (@decimal numeric, $query:expr, $value:expr) => {
        $query.bind($value.clone())
    };
    (@decimal decimal_text, $query:expr, $value:expr) => {
        $query.bind($value.to_string())
    };
    ($decimal:ident, $query:expr, $params:expr) => {{
        let mut query = $query;
        for value in $params {
            query = match value {
                DbValue::Null => query.bind(Option::<String>::None),
                DbValue::Boolean(value) => query.bind(*value),
                DbValue::Integer(value) => query.bind(*value),
                DbValue::Real(value) => query.bind(*value),
                DbValue::Text(value) => query.bind(value.clone()),
                DbValue::Blob(value) => query.bind(value.clone()),
                DbValue::Timestamp(value) => query.bind(*value),
                DbValue::Uuid(value) => query.bind(*value),
                DbValue::Decimal(value) => bind_query_values!(@decimal $decimal, query, value),
                DbValue::Json(value) => query.bind(value.clone()),
            };
        }
        query
    }};
}

#[cfg(all(not(target_arch = "wasm32"), feature = "postgres"))]
async fn execute_postgres(
    pool: &sqlx::PgPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    execute_postgres_with(pool, query, params).await
}

#[cfg(all(not(target_arch = "wasm32"), feature = "postgres"))]
async fn execute_postgres_with<'e, E>(
    executor: E,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let query = bind_query_values!(numeric, sqlx::query(query), params);
    let result = query.execute(executor).await?;
    Ok(DbExecResult {
        rows: Vec::new(),
        rows_read: 0,
        rows_written: result.rows_affected(),
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "mysql"))]
async fn execute_mysql(
    pool: &sqlx::MySqlPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    execute_mysql_with(pool, query, params).await
}

#[cfg(all(not(target_arch = "wasm32"), feature = "mysql"))]
async fn execute_mysql_with<'e, E>(
    executor: E,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::MySql>,
{
    let query = bind_query_values!(numeric, sqlx::query(query), params);
    let result = query.execute(executor).await?;
    Ok(DbExecResult {
        rows: Vec::new(),
        rows_read: 0,
        rows_written: result.rows_affected(),
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
async fn execute_sqlite(
    pool: &sqlx::SqlitePool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    execute_sqlite_with(pool, query, params).await
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
async fn execute_sqlite_with<'e, E>(
    executor: E,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let query = bind_query_values!(decimal_text, sqlx::query(query), params);
    let result = query.execute(executor).await?;
    Ok(DbExecResult {
        rows: Vec::new(),
        rows_read: 0,
        rows_written: result.rows_affected(),
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "postgres"))]
async fn query_postgres(
    pool: &sqlx::PgPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    query_postgres_with(pool, query, params).await
}

#[cfg(all(not(target_arch = "wasm32"), feature = "postgres"))]
async fn query_postgres_with<'e, E>(
    executor: E,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let query = bind_query_values!(numeric, sqlx::query(query), params);
    let rows = query.fetch_all(executor).await?;
    let rows_json = rows
        .iter()
        .map(postgres_row_to_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DbExecResult {
        rows_read: rows_json.len() as u64,
        rows: rows_json,
        rows_written: 0,
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "mysql"))]
async fn query_mysql(
    pool: &sqlx::MySqlPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    query_mysql_with(pool, query, params).await
}

#[cfg(all(not(target_arch = "wasm32"), feature = "mysql"))]
async fn query_mysql_with<'e, E>(
    executor: E,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::MySql>,
{
    let query = bind_query_values!(numeric, sqlx::query(query), params);
    let rows = query.fetch_all(executor).await?;
    let rows_json = rows
        .iter()
        .map(mysql_row_to_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DbExecResult {
        rows_read: rows_json.len() as u64,
        rows: rows_json,
        rows_written: 0,
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
async fn query_sqlite(
    pool: &sqlx::SqlitePool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    query_sqlite_with(pool, query, params).await
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
async fn query_sqlite_with<'e, E>(
    executor: E,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let query = bind_query_values!(decimal_text, sqlx::query(query), params);
    let rows = query.fetch_all(executor).await?;
    let rows_json = rows
        .iter()
        .map(sqlite_row_to_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DbExecResult {
        rows_read: rows_json.len() as u64,
        rows: rows_json,
        rows_written: 0,
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "postgres"))]
fn postgres_row_to_json(row: &sqlx::postgres::PgRow) -> Result<serde_json::Value, DbError> {
    use sqlx::{Column as _, Row as _, TypeInfo as _};

    let mut object = serde_json::Map::with_capacity(row.columns().len());
    for (index, column) in row.columns().iter().enumerate() {
        object.insert(
            sqlx::Column::name(column).to_owned(),
            postgres_value_to_json(row, index, &column.type_info().name().to_ascii_uppercase())?,
        );
    }
    Ok(serde_json::Value::Object(object))
}

#[cfg(all(not(target_arch = "wasm32"), feature = "mysql"))]
fn mysql_row_to_json(row: &sqlx::mysql::MySqlRow) -> Result<serde_json::Value, DbError> {
    use sqlx::{Column as _, Row as _, TypeInfo as _};

    let mut object = serde_json::Map::with_capacity(row.columns().len());
    for (index, column) in row.columns().iter().enumerate() {
        object.insert(
            sqlx::Column::name(column).to_owned(),
            mysql_value_to_json(row, index, &column.type_info().name().to_ascii_uppercase())?,
        );
    }
    Ok(serde_json::Value::Object(object))
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
fn sqlite_row_to_json(row: &sqlx::sqlite::SqliteRow) -> Result<serde_json::Value, DbError> {
    use sqlx::{Column as _, Row as _, TypeInfo as _};

    let mut object = serde_json::Map::with_capacity(row.columns().len());
    for (index, column) in row.columns().iter().enumerate() {
        object.insert(
            sqlx::Column::name(column).to_owned(),
            sqlite_value_to_json(row, index, &column.type_info().name().to_ascii_uppercase())?,
        );
    }
    Ok(serde_json::Value::Object(object))
}

/// Decode a column as `$ty` and render it with `$render`; if the typed decode
/// fails (e.g. an unexpected wire format), fall back to the generic
/// string-then-bytes conversion instead of failing the whole query.
#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql")
))]
macro_rules! typed_or_fallback {
    ($row:expr, $index:expr, $ty:ty, $render:expr) => {
        match $row.try_get::<Option<$ty>, _>($index) {
            Ok(value) => option_to_json(value.map($render)),
            Err(_) => fallback_value_to_json(
                $row.try_get::<Option<String>, _>($index),
                $row.try_get::<Option<Vec<u8>>, _>($index),
            ),
        }
    };
}

#[cfg(all(not(target_arch = "wasm32"), feature = "postgres"))]
fn postgres_value_to_json(
    row: &sqlx::postgres::PgRow,
    index: usize,
    type_name: &str,
) -> Result<serde_json::Value, DbError> {
    use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

    match type_name {
        "BOOL" => Ok(option_to_json(row.try_get::<Option<bool>, _>(index)?)),
        "INT2" => Ok(option_to_json(
            row.try_get::<Option<i16>, _>(index)?.map(i64::from),
        )),
        "INT4" => Ok(option_to_json(
            row.try_get::<Option<i32>, _>(index)?.map(i64::from),
        )),
        "INT8" | "OID" => Ok(option_to_json(row.try_get::<Option<i64>, _>(index)?)),
        "FLOAT4" => Ok(option_to_json(
            row.try_get::<Option<f32>, _>(index)?.map(f64::from),
        )),
        "FLOAT8" => Ok(option_to_json(row.try_get::<Option<f64>, _>(index)?)),
        "BYTEA" => Ok(option_to_json(row.try_get::<Option<Vec<u8>>, _>(index)?)),
        "JSON" | "JSONB" => Ok(option_to_json(
            row.try_get::<Option<serde_json::Value>, _>(index)?,
        )),
        "TIMESTAMPTZ" => {
            Ok(typed_or_fallback!(row, index, DateTime<Utc>, |value| value.to_rfc3339()))
        }
        "TIMESTAMP" => Ok(typed_or_fallback!(row, index, NaiveDateTime, |value| value.to_string())),
        "DATE" => Ok(typed_or_fallback!(row, index, NaiveDate, |value| value.to_string())),
        "TIME" => Ok(typed_or_fallback!(row, index, NaiveTime, |value| value.to_string())),
        "UUID" => Ok(typed_or_fallback!(row, index, sqlx::types::Uuid, |value| {
            value.to_string()
        })),
        "NUMERIC" => Ok(typed_or_fallback!(
            row,
            index,
            sqlx::types::BigDecimal,
            |value| value.to_string()
        )),
        _ => Ok(fallback_value_to_json(
            row.try_get::<Option<String>, _>(index),
            row.try_get::<Option<Vec<u8>>, _>(index),
        )),
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "mysql"))]
fn mysql_value_to_json(
    row: &sqlx::mysql::MySqlRow,
    index: usize,
    type_name: &str,
) -> Result<serde_json::Value, DbError> {
    use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

    match type_name {
        "BOOLEAN" | "BOOL" => Ok(option_to_json(row.try_get::<Option<bool>, _>(index)?)),
        "SMALLINT" => Ok(option_to_json(
            row.try_get::<Option<i16>, _>(index)?.map(i64::from),
        )),
        "INT" | "INTEGER" | "MEDIUMINT" | "TINYINT" => Ok(option_to_json(
            row.try_get::<Option<i32>, _>(index)?.map(i64::from),
        )),
        "BIGINT" => Ok(option_to_json(row.try_get::<Option<i64>, _>(index)?)),
        "TINYINT UNSIGNED" => Ok(typed_or_fallback!(row, index, u8, u64::from)),
        "SMALLINT UNSIGNED" | "YEAR" => Ok(typed_or_fallback!(row, index, u16, u64::from)),
        "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => Ok(typed_or_fallback!(row, index, u32, u64::from)),
        "BIGINT UNSIGNED" => Ok(typed_or_fallback!(row, index, u64, |value| value)),
        "FLOAT" => Ok(option_to_json(
            row.try_get::<Option<f32>, _>(index)?.map(f64::from),
        )),
        "DOUBLE" => Ok(option_to_json(row.try_get::<Option<f64>, _>(index)?)),
        "DECIMAL" => Ok(typed_or_fallback!(
            row,
            index,
            sqlx::types::BigDecimal,
            |value| value.to_string()
        )),
        "TIMESTAMP" => {
            Ok(typed_or_fallback!(row, index, DateTime<Utc>, |value| value.to_rfc3339()))
        }
        "DATETIME" => Ok(typed_or_fallback!(row, index, NaiveDateTime, |value| value.to_string())),
        "DATE" => Ok(typed_or_fallback!(row, index, NaiveDate, |value| value.to_string())),
        "TIME" => Ok(typed_or_fallback!(row, index, NaiveTime, |value| value.to_string())),
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" => {
            Ok(option_to_json(row.try_get::<Option<Vec<u8>>, _>(index)?))
        }
        "JSON" => Ok(option_to_json(
            row.try_get::<Option<serde_json::Value>, _>(index)?,
        )),
        _ => Ok(fallback_value_to_json(
            row.try_get::<Option<String>, _>(index),
            row.try_get::<Option<Vec<u8>>, _>(index),
        )),
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
fn sqlite_value_to_json(
    row: &sqlx::sqlite::SqliteRow,
    index: usize,
    type_name: &str,
) -> Result<serde_json::Value, DbError> {
    match type_name {
        "BOOLEAN" | "BOOL" => Ok(option_to_json(row.try_get::<Option<bool>, _>(index)?)),
        "INTEGER" | "INT" => Ok(option_to_json(row.try_get::<Option<i64>, _>(index)?)),
        "REAL" | "FLOAT" | "DOUBLE" => Ok(option_to_json(row.try_get::<Option<f64>, _>(index)?)),
        "BLOB" => Ok(option_to_json(row.try_get::<Option<Vec<u8>>, _>(index)?)),
        "TEXT" => Ok(option_to_json(row.try_get::<Option<String>, _>(index)?)),
        // SQLite stores dates and times as TEXT/INTEGER/REAL; render the text
        // form when possible and fall back to dynamic typing otherwise.
        "DATETIME" | "DATE" | "TIME" | "TIMESTAMP" => Ok(row
            .try_get::<Option<String>, _>(index)
            .map_or_else(|_| sqlite_dynamic_value_to_json(row, index), option_to_json)),
        _ => Ok(sqlite_dynamic_value_to_json(row, index)),
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "sqlite"))]
fn sqlite_dynamic_value_to_json(row: &sqlx::sqlite::SqliteRow, index: usize) -> serde_json::Value {
    if let Ok(value) = row.try_get::<Option<i64>, _>(index) {
        return option_to_json(value);
    }
    if let Ok(value) = row.try_get::<Option<f64>, _>(index) {
        return option_to_json(value);
    }
    if let Ok(value) = row.try_get::<Option<String>, _>(index) {
        return option_to_json(value);
    }
    if let Ok(value) = row.try_get::<Option<Vec<u8>>, _>(index) {
        return option_to_json(value);
    }
    serde_json::Value::Null
}

/// Convert a column that has no dedicated match arm into JSON.
///
/// Tries a string decode first and, **only if that decode fails** (rather than
/// aborting the whole query as older versions did), falls back to raw bytes.
/// If both decodes fail the value is rendered as `null`.
#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql")
))]
fn fallback_value_to_json(
    string_value: Result<Option<String>, sqlx::Error>,
    bytes_value: Result<Option<Vec<u8>>, sqlx::Error>,
) -> serde_json::Value {
    string_value.map_or_else(
        |_| bytes_value.map_or(serde_json::Value::Null, option_to_json),
        option_to_json,
    )
}

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
fn option_to_json<T>(value: Option<T>) -> serde_json::Value
where
    T: serde::Serialize,
{
    value.map_or(serde_json::Value::Null, |value| serde_json::json!(value))
}

#[cfg(test)]
mod single_row_tests {
    use super::{append_single_row_limit, DbDialect};

    fn limited(sql: &str) -> String {
        append_single_row_limit(sql, DbDialect::Postgres)
            .expect("statement should tokenize")
            .into_owned()
    }

    #[test]
    fn a_plain_select_gets_a_limit() {
        assert_eq!(
            limited("SELECT * FROM events WHERE user_id = $1"),
            "SELECT * FROM events WHERE user_id = $1\nLIMIT 1"
        );
    }

    #[test]
    fn a_cte_query_gets_a_limit() {
        assert_eq!(
            limited("WITH recent AS (SELECT 1) SELECT * FROM recent"),
            "WITH recent AS (SELECT 1) SELECT * FROM recent\nLIMIT 1"
        );
    }

    #[test]
    fn the_limit_goes_before_a_trailing_semicolon() {
        assert_eq!(
            limited("SELECT * FROM events;  "),
            "SELECT * FROM events\nLIMIT 1"
        );
    }

    #[test]
    fn the_limit_survives_a_trailing_line_comment() {
        let rendered = limited("SELECT * FROM events -- newest first");
        assert!(rendered.ends_with("\nLIMIT 1"), "{rendered}");
    }

    #[test]
    fn a_statement_that_already_bounds_itself_is_left_alone() {
        for sql in [
            "SELECT * FROM events LIMIT 10",
            "SELECT * FROM events OFFSET 5",
            "SELECT * FROM events FETCH FIRST 3 ROWS ONLY",
            "SELECT TOP 1 * FROM events",
        ] {
            assert_eq!(limited(sql), sql, "{sql}");
        }
    }

    #[test]
    fn a_clause_that_must_follow_limit_blocks_the_rewrite() {
        // `LIMIT` has to precede `FOR UPDATE`, so appending would be a syntax error.
        for sql in [
            "SELECT * FROM events FOR UPDATE",
            "SELECT * FROM events FOR SHARE",
            "SELECT id INTO archive FROM events",
        ] {
            assert_eq!(limited(sql), sql, "{sql}");
        }
    }

    #[test]
    fn only_a_query_is_rewritten() {
        for sql in [
            "INSERT INTO events (id) VALUES ($1) RETURNING id",
            "UPDATE events SET seen = true RETURNING id",
            "DELETE FROM events RETURNING id",
            "CALL do_something()",
        ] {
            assert_eq!(limited(sql), sql, "{sql}");
        }
    }

    #[test]
    fn a_multi_statement_string_is_never_rewritten() {
        let sql = "SELECT 1; SELECT 2";
        assert_eq!(limited(sql), sql);
    }

    #[test]
    fn every_dialect_accepts_the_same_clause() {
        for dialect in [DbDialect::Postgres, DbDialect::MySql, DbDialect::Sqlite] {
            let rendered = append_single_row_limit("SELECT * FROM t", dialect)
                .expect("statement should tokenize");
            assert_eq!(rendered, "SELECT * FROM t\nLIMIT 1");
        }
    }
}

#[cfg(test)]
mod db_value_tests {
    use super::DbValue;
    use bigdecimal::BigDecimal;
    use chrono::{DateTime, Utc};
    use core::str::FromStr;
    use uuid::Uuid;

    #[test]
    fn rich_types_convert_without_being_stringified_by_the_caller() {
        let timestamp = DateTime::<Utc>::from_str("2024-05-06T07:08:09Z").expect("valid RFC 3339");
        assert!(matches!(DbValue::from(timestamp), DbValue::Timestamp(_)));

        let id = Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").expect("valid UUID");
        assert!(matches!(DbValue::from(id), DbValue::Uuid(_)));

        let amount = BigDecimal::from_str("19.99").expect("valid decimal");
        assert!(matches!(DbValue::from(amount), DbValue::Decimal(_)));

        let document = serde_json::json!({ "kind": "email" });
        assert!(matches!(DbValue::from(document), DbValue::Json(_)));
    }

    #[test]
    fn an_absent_rich_value_still_binds_as_null() {
        let missing: Option<Uuid> = None;
        assert!(matches!(DbValue::from(missing), DbValue::Null));
    }
}

#[cfg(test)]
mod prepare_tests {
    use super::{prepare_query_sql, DbDialect, DbError};

    #[test]
    fn counts_and_keeps_question_mark_placeholders_for_sqlite() {
        let sql = "SELECT * FROM t WHERE a = ? AND b = ?";
        let rendered = prepare_query_sql(sql, 2, DbDialect::Sqlite).expect("query should prepare");
        assert_eq!(rendered, sql);
    }

    #[test]
    fn rewrites_placeholders_to_numbered_form_for_postgres() {
        let rendered = prepare_query_sql(
            "SELECT * FROM t WHERE a = ? AND b = ?",
            2,
            DbDialect::Postgres,
        )
        .expect("query should prepare");
        assert_eq!(rendered, "SELECT * FROM t WHERE a = $1 AND b = $2");
    }

    #[test]
    fn reports_parameter_count_mismatch() {
        let error = prepare_query_sql(
            "SELECT * FROM t WHERE a = ? AND b = ?",
            1,
            DbDialect::Sqlite,
        )
        .expect_err("mismatched parameter count should fail");
        match error {
            DbError::ParameterCountMismatch { expected, actual } => {
                assert_eq!(expected, 2);
                assert_eq!(actual, 1);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        // The message should steer users away from `$1`-style placeholders.
        assert!(error_to_string_mentions_placeholder_hint());
    }

    fn error_to_string_mentions_placeholder_hint() -> bool {
        let error = DbError::ParameterCountMismatch {
            expected: 2,
            actual: 1,
        };
        error.to_string().contains("`?` placeholders")
    }

    #[test]
    fn question_mark_inside_string_literal_is_not_a_placeholder() {
        let sql = "SELECT 'a?b' AS label FROM t WHERE x = ?";
        let rendered = prepare_query_sql(sql, 1, DbDialect::Postgres).expect("should prepare");
        assert_eq!(rendered, "SELECT 'a?b' AS label FROM t WHERE x = $1");
    }

    #[test]
    fn question_mark_inside_comment_is_not_a_placeholder() {
        let sql = "SELECT ? AS v -- what?\nFROM t";
        let rendered = prepare_query_sql(sql, 1, DbDialect::Sqlite).expect("should prepare");
        assert_eq!(rendered, sql);
    }

    #[test]
    fn multibyte_content_is_preserved_during_rewrite() {
        let sql = "SELECT 'héllo → 世界' AS greeting, ? AS value";
        let rendered = prepare_query_sql(sql, 1, DbDialect::Postgres).expect("should prepare");
        assert_eq!(rendered, "SELECT 'héllo → 世界' AS greeting, $1 AS value");
    }

    #[test]
    fn multiline_multibyte_queries_map_locations_correctly() {
        let sql = "SELECT '日本語',\n       ?,\n       'ещё',\n       ?\nFROM t";
        let rendered = prepare_query_sql(sql, 2, DbDialect::Postgres).expect("should prepare");
        assert_eq!(
            rendered,
            "SELECT '日本語',\n       $1,\n       'ещё',\n       $2\nFROM t"
        );
    }
}

#[cfg(all(
    test,
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql")
))]
mod fallback_tests {
    use super::fallback_value_to_json;

    #[test]
    fn string_decode_failure_falls_back_to_bytes() {
        let value = fallback_value_to_json(
            Err(sqlx::Error::RowNotFound),
            Ok(Some(vec![1_u8, 2_u8, 3_u8])),
        );
        assert_eq!(value, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn successful_string_decode_is_rendered_as_string() {
        let value =
            fallback_value_to_json(Ok(Some("hello".to_owned())), Err(sqlx::Error::RowNotFound));
        assert_eq!(value, serde_json::json!("hello"));
    }

    #[test]
    fn sql_null_is_rendered_as_json_null() {
        let value = fallback_value_to_json(Ok(None), Ok(None));
        assert!(value.is_null());
    }

    #[test]
    fn both_decode_failures_render_null_instead_of_failing_the_query() {
        let value =
            fallback_value_to_json(Err(sqlx::Error::RowNotFound), Err(sqlx::Error::RowNotFound));
        assert!(value.is_null());
    }
}

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
mod tests {
    use super::{BatchStatement, Db};

    #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
    struct CountRow {
        count: i64,
    }

    #[tokio::test]
    async fn execute_batch_commits_every_statement_and_returns_selected_rows() {
        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");
        db.query("CREATE TABLE entries (value INTEGER NOT NULL)")
            .execute()
            .await
            .expect("schema should be created");

        let results = db
            .execute_batch(vec![
                BatchStatement::new("INSERT INTO entries (value) VALUES (?)").bind(1_i64),
                BatchStatement::new("INSERT INTO entries (value) VALUES (?)").bind(2_i64),
                BatchStatement::new("SELECT COUNT(*) AS count FROM entries"),
            ])
            .await
            .expect("batch should commit");

        assert_eq!(results.len(), 3);
        // A `SELECT` inside a batch hands its rows back, matching D1's `batch()`.
        assert_eq!(results[2].rows, vec![serde_json::json!({ "count": 2_i64 })]);

        let row = db
            .query("SELECT COUNT(*) AS count FROM entries")
            .fetch_one::<CountRow>()
            .await
            .expect("count query should succeed");
        assert_eq!(row, CountRow { count: 2 });
    }

    #[tokio::test]
    async fn execute_batch_rolls_back_every_statement_when_one_fails() {
        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");
        db.query("CREATE TABLE entries (value INTEGER NOT NULL)")
            .execute()
            .await
            .expect("schema should be created");

        db.execute_batch(vec![
            BatchStatement::new("INSERT INTO entries (value) VALUES (?)").bind(1_i64),
            BatchStatement::new("INSERT INTO absent_table (value) VALUES (?)").bind(2_i64),
        ])
        .await
        .expect_err("a statement against a missing table should fail the batch");

        let row = db
            .query("SELECT COUNT(*) AS count FROM entries")
            .fetch_one::<CountRow>()
            .await
            .expect("count query should succeed");
        // The first insert is discarded with the rest: the batch is all-or-nothing.
        assert_eq!(row, CountRow { count: 0 });
    }

    #[tokio::test]
    async fn execute_batch_checks_placeholders_against_bound_values() {
        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");

        let error = db
            .execute_batch(vec![BatchStatement::new(
                "INSERT INTO entries (value) VALUES (?)",
            )])
            .await
            .expect_err("a statement with an unbound placeholder should be rejected");

        assert!(matches!(
            error,
            super::DbError::ParameterCountMismatch {
                expected: 1,
                actual: 0
            }
        ));
    }

    #[tokio::test]
    async fn sqlite_transaction_commit_persists_changes() {
        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");
        db.query("CREATE TABLE entries (value INTEGER NOT NULL)")
            .execute()
            .await
            .expect("schema should be created");

        let mut tx = db.begin().await.expect("transaction should begin");
        tx.query("INSERT INTO entries (value) VALUES (?)")
            .bind(1_i64)
            .execute()
            .await
            .expect("insert should succeed");
        tx.commit().await.expect("commit should succeed");

        let row = db
            .query("SELECT COUNT(*) AS count FROM entries")
            .fetch_one::<CountRow>()
            .await
            .expect("count query should succeed");
        assert_eq!(row, CountRow { count: 1 });
    }

    #[tokio::test]
    async fn sqlite_transaction_rollback_discards_changes() {
        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");
        db.query("CREATE TABLE entries (value INTEGER NOT NULL)")
            .execute()
            .await
            .expect("schema should be created");

        let mut tx = db.begin().await.expect("transaction should begin");
        tx.query("INSERT INTO entries (value) VALUES (?)")
            .bind(1_i64)
            .execute()
            .await
            .expect("insert should succeed");
        tx.rollback().await.expect("rollback should succeed");

        let row = db
            .query("SELECT COUNT(*) AS count FROM entries")
            .fetch_one::<CountRow>()
            .await
            .expect("count query should succeed");
        assert_eq!(row, CountRow { count: 0 });
    }

    #[tokio::test]
    async fn sqlite_binds_the_rich_parameter_types() {
        use bigdecimal::BigDecimal;
        use chrono::{DateTime, Utc};
        use core::str::FromStr as _;
        use uuid::Uuid;

        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");
        db.query(
            "CREATE TABLE orders (
                placed_at TEXT,
                id BLOB,
                amount TEXT,
                payload TEXT
            )",
        )
        .execute()
        .await
        .expect("schema should be created");

        let placed_at = DateTime::<Utc>::from_str("2024-05-06T07:08:09Z").expect("valid RFC 3339");
        let id = Uuid::from_str("550e8400-e29b-41d4-a716-446655440000").expect("valid UUID");
        db.query("INSERT INTO orders (placed_at, id, amount, payload) VALUES (?, ?, ?, ?)")
            .bind(placed_at)
            .bind(id)
            .bind(BigDecimal::from_str("19.99").expect("valid decimal"))
            .bind(serde_json::json!({ "kind": "email" }))
            .execute()
            .await
            .expect("insert should succeed");

        let row: serde_json::Value = db
            .query("SELECT amount FROM orders")
            .fetch_one()
            .await
            .expect("select should succeed");
        // SQLite has no decimal type, so the exact value is stored as its own rendering rather
        // than being rounded through a float.
        assert_eq!(row["amount"], serde_json::json!("19.99"));
    }

    #[tokio::test]
    async fn fetch_optional_stops_at_the_first_row() {
        #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
        struct ValueRow {
            value: i64,
        }

        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");
        db.query("CREATE TABLE entries (value INTEGER NOT NULL)")
            .execute()
            .await
            .expect("schema should be created");
        for value in 1..=3_i64 {
            db.query("INSERT INTO entries (value) VALUES (?)")
                .bind(value)
                .execute()
                .await
                .expect("insert should succeed");
        }

        // The injected `LIMIT 1` means the backend returns one row, not three.
        let first: ValueRow = db
            .query("SELECT value FROM entries ORDER BY value")
            .fetch_one()
            .await
            .expect("select should succeed");
        assert_eq!(first, ValueRow { value: 1 });

        // A statement that bounds itself is left alone and still yields its own first row.
        let bounded: Option<ValueRow> = db
            .query("SELECT value FROM entries ORDER BY value DESC LIMIT 2")
            .fetch_optional()
            .await
            .expect("select should succeed");
        assert_eq!(bounded, Some(ValueRow { value: 3 }));
    }

    #[tokio::test]
    async fn sqlite_rows_convert_to_json_covering_all_storage_classes() {
        let db = Db::connect_sqlite_memory()
            .await
            .expect("in-memory sqlite should connect");
        db.query(
            "CREATE TABLE items (
                int_col INTEGER,
                real_col REAL,
                text_col TEXT,
                blob_col BLOB,
                null_col TEXT,
                datetime_col DATETIME,
                uuid_col TEXT
            )",
        )
        .execute()
        .await
        .expect("schema should be created");

        db.query(
            "INSERT INTO items (int_col, real_col, text_col, blob_col, null_col, datetime_col, uuid_col)
             VALUES (?, ?, ?, ?, NULL, ?, ?)",
        )
        .bind(7_i64)
        .bind(2.5_f64)
        .bind("héllo 世界")
        .bind(vec![1_u8, 2_u8, 3_u8])
        .bind("2024-05-06 07:08:09")
        .bind("550e8400-e29b-41d4-a716-446655440000")
        .execute()
        .await
        .expect("insert should succeed");

        let row: serde_json::Value = db
            .query("SELECT * FROM items")
            .fetch_one()
            .await
            .expect("select should succeed");

        assert_eq!(row["int_col"], serde_json::json!(7));
        assert_eq!(row["real_col"], serde_json::json!(2.5));
        assert_eq!(row["text_col"], serde_json::json!("héllo 世界"));
        assert_eq!(row["blob_col"], serde_json::json!([1, 2, 3]));
        assert!(row["null_col"].is_null());
        // DATETIME columns render as their textual form instead of erroring.
        assert_eq!(
            row["datetime_col"],
            serde_json::json!("2024-05-06 07:08:09")
        );
        assert_eq!(
            row["uuid_col"],
            serde_json::json!("550e8400-e29b-41d4-a716-446655440000")
        );
    }
}
