//! Unified SQL database abstraction.

use std::{borrow::Cow, convert::Infallible, future::Future};

use http_kit::{
    http_error,
    middleware::{Middleware, MiddlewareError},
    Endpoint, Response,
};
use serde::de::DeserializeOwned;
use skyzen_core::{Extractor, StatusCode};
use sqlparser::{
    dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect},
    tokenizer::{Location, Token, TokenWithSpan, Tokenizer},
};

use crate::maybe_send::{BoxFuture, MaybeSend};

#[cfg(not(target_arch = "wasm32"))]
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
    #[error("database parameter count mismatch: expected {expected}, got {actual}")]
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

#[cfg(not(target_arch = "wasm32"))]
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

impl Clone for Db {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Create a new `Db` from any [`DbBackend`] implementation.
    pub fn new(store: impl DbBackend) -> Self {
        Self(Box::new(store))
    }

    /// Start building a SQL query.
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

    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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

http_error!(
    /// The database was not found in request extensions.
    pub DbNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Database not configured. Ensure a DbBackend implementation is injected."
);

impl Extractor for Db {
    type Error = DbNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(DbNotConfigured::new())
    }
}

impl Middleware for Db {
    type Error = Infallible;

    async fn handle<N: Endpoint>(
        &mut self,
        request: &mut http_kit::Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        request.extensions_mut().insert(self.clone());
        next.respond(request)
            .await
            .map_err(MiddlewareError::Endpoint)
    }
}

fn prepare_query_sql(
    query: &str,
    actual_params: usize,
    dialect: DbDialect,
) -> Result<String, DbError> {
    let tokens = tokenize_sql(query, dialect)?;
    let mut expected_params = 0usize;
    let mut cursor = 0usize;
    let mut rendered = String::with_capacity(query.len() + actual_params.saturating_mul(2));

    for token in tokens {
        let start = location_to_byte_index(query, token.span.start);
        let end = location_to_byte_index(query, token.span.end);
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
        } else {
            rendered.push_str(&query[start..end]);
        }

        cursor = end;
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

fn location_to_byte_index(query: &str, target: Location) -> usize {
    if target.line == 0 && target.column == 0 {
        return 0;
    }

    if target.line == 1 && target.column == 1 {
        return 0;
    }

    let mut line = 1u64;
    let mut column = 1u64;

    for (index, ch) in query.char_indices() {
        if line == target.line && column == target.column {
            return index;
        }

        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }

    query.len()
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
enum NativeDbBackend {
    Postgres(sqlx::PgPool),
    MySql(sqlx::MySqlPool),
    Sqlite(sqlx::SqlitePool),
}

#[cfg(not(target_arch = "wasm32"))]
enum NativeDbTransaction {
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    MySql(sqlx::Transaction<'static, sqlx::MySql>),
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
}

#[cfg(not(target_arch = "wasm32"))]
impl DbBackend for NativeDbBackend {
    fn dialect(&self) -> DbDialect {
        match self {
            Self::Postgres(_) => DbDialect::Postgres,
            Self::MySql(_) => DbDialect::MySql,
            Self::Sqlite(_) => DbDialect::Sqlite,
        }
    }

    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            Self::Postgres(pool) => query_postgres(pool, query, params).await,
            Self::MySql(pool) => query_mysql(pool, query, params).await,
            Self::Sqlite(pool) => query_sqlite(pool, query, params).await,
        }
    }

    async fn execute(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            Self::Postgres(pool) => execute_postgres(pool, query, params).await,
            Self::MySql(pool) => execute_mysql(pool, query, params).await,
            Self::Sqlite(pool) => execute_sqlite(pool, query, params).await,
        }
    }

    async fn begin(&self) -> Result<DbTransaction, DbError> {
        let tx = match self {
            Self::Postgres(pool) => NativeDbTransaction::Postgres(pool.begin().await?),
            Self::MySql(pool) => NativeDbTransaction::MySql(pool.begin().await?),
            Self::Sqlite(pool) => NativeDbTransaction::Sqlite(pool.begin().await?),
        };
        Ok(DbTransaction::new(tx))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl DbTransactionBackend for NativeDbTransaction {
    fn dialect(&self) -> DbDialect {
        match self {
            Self::Postgres(_) => DbDialect::Postgres,
            Self::MySql(_) => DbDialect::MySql,
            Self::Sqlite(_) => DbDialect::Sqlite,
        }
    }

    async fn query(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            Self::Postgres(tx) => query_postgres_with(&mut **tx, query, params).await,
            Self::MySql(tx) => query_mysql_with(&mut **tx, query, params).await,
            Self::Sqlite(tx) => query_sqlite_with(&mut **tx, query, params).await,
        }
    }

    async fn execute(&mut self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DbError> {
        match self {
            Self::Postgres(tx) => execute_postgres_with(&mut **tx, query, params).await,
            Self::MySql(tx) => execute_mysql_with(&mut **tx, query, params).await,
            Self::Sqlite(tx) => execute_sqlite_with(&mut **tx, query, params).await,
        }
    }

    async fn commit(self) -> Result<(), DbError> {
        match self {
            Self::Postgres(tx) => tx.commit().await.map_err(Into::into),
            Self::MySql(tx) => tx.commit().await.map_err(Into::into),
            Self::Sqlite(tx) => tx.commit().await.map_err(Into::into),
        }
    }

    async fn rollback(self) -> Result<(), DbError> {
        match self {
            Self::Postgres(tx) => tx.rollback().await.map_err(Into::into),
            Self::MySql(tx) => tx.rollback().await.map_err(Into::into),
            Self::Sqlite(tx) => tx.rollback().await.map_err(Into::into),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
async fn execute_postgres(
    pool: &sqlx::PgPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    execute_postgres_with(pool, query, params).await
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
async fn execute_mysql(
    pool: &sqlx::MySqlPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    execute_mysql_with(pool, query, params).await
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
async fn execute_sqlite(
    pool: &sqlx::SqlitePool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    execute_sqlite_with(pool, query, params).await
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
async fn query_postgres(
    pool: &sqlx::PgPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    query_postgres_with(pool, query, params).await
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
async fn query_mysql(
    pool: &sqlx::MySqlPool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    query_mysql_with(pool, query, params).await
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
async fn query_sqlite(
    pool: &sqlx::SqlitePool,
    query: &str,
    params: &[DbValue],
) -> Result<DbExecResult, DbError> {
    query_sqlite_with(pool, query, params).await
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn postgres_value_to_json(
    row: &sqlx::postgres::PgRow,
    index: usize,
    type_name: &str,
) -> Result<serde_json::Value, DbError> {
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
        _ => Ok(fallback_value_to_json(
            row.try_get::<Option<String>, _>(index)?,
            row.try_get::<Option<Vec<u8>>, _>(index),
        )),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn mysql_value_to_json(
    row: &sqlx::mysql::MySqlRow,
    index: usize,
    type_name: &str,
) -> Result<serde_json::Value, DbError> {
    match type_name {
        "BOOLEAN" | "BOOL" => Ok(option_to_json(row.try_get::<Option<bool>, _>(index)?)),
        "SMALLINT" => Ok(option_to_json(
            row.try_get::<Option<i16>, _>(index)?.map(i64::from),
        )),
        "INT" | "INTEGER" | "MEDIUMINT" | "TINYINT" => Ok(option_to_json(
            row.try_get::<Option<i32>, _>(index)?.map(i64::from),
        )),
        "BIGINT" => Ok(option_to_json(row.try_get::<Option<i64>, _>(index)?)),
        "FLOAT" => Ok(option_to_json(
            row.try_get::<Option<f32>, _>(index)?.map(f64::from),
        )),
        "DOUBLE" | "DECIMAL" => Ok(option_to_json(row.try_get::<Option<f64>, _>(index)?)),
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" => {
            Ok(option_to_json(row.try_get::<Option<Vec<u8>>, _>(index)?))
        }
        "JSON" => Ok(option_to_json(
            row.try_get::<Option<serde_json::Value>, _>(index)?,
        )),
        _ => Ok(fallback_value_to_json(
            row.try_get::<Option<String>, _>(index)?,
            row.try_get::<Option<Vec<u8>>, _>(index),
        )),
    }
}

#[cfg(not(target_arch = "wasm32"))]
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
        _ => Ok(sqlite_dynamic_value_to_json(row, index)),
    }
}

#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(not(target_arch = "wasm32"))]
fn fallback_value_to_json(
    string_value: Option<String>,
    bytes_value: Result<Option<Vec<u8>>, sqlx::Error>,
) -> serde_json::Value {
    if let Some(value) = string_value {
        return serde_json::Value::String(value);
    }

    if let Ok(value) = bytes_value {
        return option_to_json(value);
    }

    serde_json::Value::Null
}

#[cfg(not(target_arch = "wasm32"))]
fn option_to_json<T>(value: Option<T>) -> serde_json::Value
where
    T: serde::Serialize,
{
    value.map_or(serde_json::Value::Null, |value| serde_json::json!(value))
}

#[cfg(all(
    test,
    not(target_arch = "wasm32"),
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
}
