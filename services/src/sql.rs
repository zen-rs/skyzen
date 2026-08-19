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

use std::{borrow::Cow, future::Future};

use serde::de::DeserializeOwned;
use sqlparser::{
    dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
    tokenizer::{Location, Token, TokenWithSpan, Tokenizer},
};

use crate::maybe_send::{BoxFuture, MaybeSend};

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
use sqlx::Row;

/// Errors from database operations.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The underlying database backend returned an error.
    #[error("database error: {0}")]
    Backend(String),

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
}

#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "postgres", feature = "mysql", feature = "sqlite")
))]
impl From<sqlx::Error> for DbError {
    fn from(error: sqlx::Error) -> Self {
        Self::Backend(error.to_string())
    }
}

/// A SQL parameter value.
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
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + MaybeSend;

    /// Execute a statement that does not return rows.
    fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + MaybeSend;

    /// Begin a database transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot create a transaction.
    fn begin(&self) -> impl Future<Output = Result<DbTransaction, DbError>> + MaybeSend {
        async { Err(DbError::TransactionsUnsupported) }
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
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + MaybeSend;

    /// Execute a statement that does not return rows inside this transaction.
    fn execute(
        &mut self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DbError>> + MaybeSend;

    /// Commit this transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot commit the transaction.
    fn commit(self) -> impl Future<Output = Result<(), DbError>> + MaybeSend
    where
        Self: Sized;

    /// Roll back this transaction.
    fn rollback(self) -> impl Future<Output = Result<(), DbError>> + MaybeSend
    where
        Self: Sized;
}

trait DbBackendObj: Send + Sync {
    fn dialect(&self) -> DbDialect;
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>>;
    fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>>;
    fn begin(&self) -> BoxFuture<'_, Result<DbTransaction, DbError>>;
    fn clone_box(&self) -> Box<dyn DbBackendObj>;
}

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

impl<T: DbBackend> DbBackendObj for T {
    fn dialect(&self) -> DbDialect {
        DbBackend::dialect(self)
    }

    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>> {
        Box::pin(DbBackend::query(self, query, params))
    }

    fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DbError>> {
        Box::pin(DbBackend::execute(self, query, params))
    }

    fn begin(&self) -> BoxFuture<'_, Result<DbTransaction, DbError>> {
        Box::pin(DbBackend::begin(self))
    }

    fn clone_box(&self) -> Box<dyn DbBackendObj> {
        Box::new(self.clone())
    }
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
        DbQuery {
            db: self,
            sql: Cow::Borrowed(sql),
            params: Vec::new(),
        }
    }

    /// Begin a database transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot create a transaction.
    pub async fn begin(&self) -> Result<DbTransaction, DbError> {
        self.0.begin().await
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
        DbTransactionQuery {
            tx: self,
            sql: Cow::Borrowed(sql),
            params: Vec::new(),
        }
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

/// A query builder for [`Db`].
#[derive(Debug)]
pub struct DbQuery<'a> {
    db: &'a Db,
    sql: Cow<'a, str>,
    params: Vec<DbValue>,
}

impl DbQuery<'_> {
    /// Bind a parameter value to the query.
    #[must_use]
    pub fn bind<T>(mut self, value: T) -> Self
    where
        T: Into<DbValue>,
    {
        self.params.push(value.into());
        self
    }

    /// Execute a statement that does not return rows.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or result
    /// conversion fails.
    pub async fn execute(self) -> Result<DbExecResult, DbError> {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.db.0.dialect())?;
        self.db.0.execute(&sql, &self.params).await
    }

    /// Execute a query and deserialize all rows into `T`.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_all<T>(self) -> Result<Vec<T>, DbError>
    where
        T: DeserializeOwned,
    {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.db.0.dialect())?;
        let result = self.db.0.query(&sql, &self.params).await?;
        result
            .rows
            .into_iter()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Execute a query and deserialize the first row into `T`, if present.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_optional<T>(self) -> Result<Option<T>, DbError>
    where
        T: DeserializeOwned,
    {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.db.0.dialect())?;
        let result = self.db.0.query(&sql, &self.params).await?;
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
    pub async fn fetch_one<T>(self) -> Result<T, DbError>
    where
        T: DeserializeOwned,
    {
        self.fetch_optional().await?.ok_or(DbError::RowNotFound)
    }
}

/// A query builder scoped to a mutable transaction.
#[derive(Debug)]
pub struct DbTransactionQuery<'a> {
    tx: &'a mut DbTransaction,
    sql: Cow<'a, str>,
    params: Vec<DbValue>,
}

impl DbTransactionQuery<'_> {
    /// Bind a parameter value to the query.
    #[must_use]
    pub fn bind<T>(mut self, value: T) -> Self
    where
        T: Into<DbValue>,
    {
        self.params.push(value.into());
        self
    }

    /// Execute a statement that does not return rows.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or result
    /// conversion fails.
    pub async fn execute(self) -> Result<DbExecResult, DbError> {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.tx.0.dialect())?;
        self.tx.0.execute(&sql, &self.params).await
    }

    /// Execute a query and deserialize all rows into `T`.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_all<T>(self) -> Result<Vec<T>, DbError>
    where
        T: DeserializeOwned,
    {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.tx.0.dialect())?;
        let result = self.tx.0.query(&sql, &self.params).await?;
        result
            .rows
            .into_iter()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Execute a query and deserialize the first row into `T`, if present.
    ///
    /// # Errors
    ///
    /// Returns an error if placeholder rewriting, backend execution, or row
    /// deserialization fails.
    pub async fn fetch_optional<T>(self) -> Result<Option<T>, DbError>
    where
        T: DeserializeOwned,
    {
        let sql = prepare_query_sql(self.sql.as_ref(), self.params.len(), self.tx.0.dialect())?;
        let result = self.tx.0.query(&sql, &self.params).await?;
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
    pub async fn fetch_one<T>(self) -> Result<T, DbError>
    where
        T: DeserializeOwned,
    {
        self.fetch_optional().await?.ok_or(DbError::RowNotFound)
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
macro_rules! bind_query_values {
    ($query:expr, $params:expr) => {{
        let mut query = $query;
        for value in $params {
            query = match value {
                DbValue::Null => query.bind(Option::<String>::None),
                DbValue::Boolean(value) => query.bind(*value),
                DbValue::Integer(value) => query.bind(*value),
                DbValue::Real(value) => query.bind(*value),
                DbValue::Text(value) => query.bind(value.clone()),
                DbValue::Blob(value) => query.bind(value.clone()),
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
    let query = bind_query_values!(sqlx::query(query), params);
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
    let query = bind_query_values!(sqlx::query(query), params);
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
    let query = bind_query_values!(sqlx::query(query), params);
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
    let query = bind_query_values!(sqlx::query(query), params);
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
    let query = bind_query_values!(sqlx::query(query), params);
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
    let query = bind_query_values!(sqlx::query(query), params);
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
    use super::Db;

    #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
    struct CountRow {
        count: i64,
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
