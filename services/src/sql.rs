//! Portable SQL database abstraction.

use core::{convert::Infallible, future::Future};

use http_kit::{
    http_error,
    middleware::{Middleware, MiddlewareError},
    Endpoint, Response,
};
use serde::de::DeserializeOwned;
use skyzen_core::{Extractor, StatusCode};

use crate::maybe_send::{BoxFuture, MaybeSend};

/// Errors from portable SQL operations.
#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    /// The underlying database backend returned an error.
    #[error("sql error: {0}")]
    Backend(String),

    /// Serialization or deserialization failed.
    #[error("sql serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// A SQL parameter value.
#[derive(Debug, Clone)]
pub enum SqlValue {
    /// A null value.
    Null,
    /// A boolean value.
    Boolean(bool),
    /// An integer value.
    Integer(i64),
    /// A floating-point value.
    Real(f64),
    /// A text value.
    Text(String),
    /// A blob value.
    Blob(Vec<u8>),
}

impl From<bool> for SqlValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<i8> for SqlValue {
    fn from(value: i8) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<i16> for SqlValue {
    fn from(value: i16) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<i32> for SqlValue {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<i64> for SqlValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<u8> for SqlValue {
    fn from(value: u8) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<u16> for SqlValue {
    fn from(value: u16) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<u32> for SqlValue {
    fn from(value: u32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<f32> for SqlValue {
    fn from(value: f32) -> Self {
        Self::Real(f64::from(value))
    }
}

impl From<f64> for SqlValue {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}

impl From<String> for SqlValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for SqlValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<Vec<u8>> for SqlValue {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value)
    }
}

impl From<&[u8]> for SqlValue {
    fn from(value: &[u8]) -> Self {
        Self::Blob(value.to_vec())
    }
}

/// Result of a SQL execution.
#[derive(Debug, Clone, Default)]
pub struct SqlResult {
    /// Rows returned by the query, each as a JSON-like map.
    pub rows: Vec<serde_json::Value>,
    /// Number of rows read.
    pub rows_read: u64,
    /// Number of rows written.
    pub rows_written: u64,
}

/// A portable SQL database interface.
pub trait SqlDatabase: Send + Sync + Clone + 'static {
    /// Execute a SQL statement with parameters.
    fn exec(
        &self,
        query: &str,
        params: &[SqlValue],
    ) -> impl Future<Output = Result<SqlResult, SqlError>> + MaybeSend;
}

trait SqlDatabaseObj: Send + Sync {
    fn exec<'a>(
        &'a self,
        query: &'a str,
        params: &'a [SqlValue],
    ) -> BoxFuture<'a, Result<SqlResult, SqlError>>;
    fn clone_box(&self) -> Box<dyn SqlDatabaseObj>;
}

impl<T: SqlDatabase> SqlDatabaseObj for T {
    fn exec<'a>(
        &'a self,
        query: &'a str,
        params: &'a [SqlValue],
    ) -> BoxFuture<'a, Result<SqlResult, SqlError>> {
        Box::pin(SqlDatabase::exec(self, query, params))
    }

    fn clone_box(&self) -> Box<dyn SqlDatabaseObj> {
        Box::new(self.clone())
    }
}

/// A type-erased portable SQL database extractor.
pub struct SqlDb(Box<dyn SqlDatabaseObj>);

impl Clone for SqlDb {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for SqlDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlDb").finish_non_exhaustive()
    }
}

impl SqlDb {
    /// Create a new `SqlDb` from any [`SqlDatabase`] implementation.
    pub fn new(store: impl SqlDatabase) -> Self {
        Self(Box::new(store))
    }

    /// Execute a SQL statement with parameters.
    ///
    /// # Errors
    ///
    /// Returns [`SqlError`] if the backend operation fails.
    pub async fn exec(&self, query: &str, params: &[SqlValue]) -> Result<SqlResult, SqlError> {
        self.0.exec(query, params).await
    }

    /// Execute a query and deserialize each row into `T`.
    ///
    /// # Errors
    ///
    /// Returns [`SqlError`] if execution or deserialization fails.
    pub async fn query<T: DeserializeOwned>(
        &self,
        query: &str,
        params: &[SqlValue],
    ) -> Result<Vec<T>, SqlError> {
        let result = self.exec(query, params).await?;
        result
            .rows
            .into_iter()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Execute a query and deserialize the first row into `T`.
    ///
    /// # Errors
    ///
    /// Returns [`SqlError`] if execution or deserialization fails.
    pub async fn query_one<T: DeserializeOwned>(
        &self,
        query: &str,
        params: &[SqlValue],
    ) -> Result<Option<T>, SqlError> {
        let result = self.exec(query, params).await?;
        result
            .rows
            .into_iter()
            .next()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .transpose()
    }
}

http_error!(
    /// The portable SQL database was not found in request extensions.
    pub SqlDbNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Portable SQL database not configured. Ensure a SqlDatabase implementation is injected."
);

impl Extractor for SqlDb {
    type Error = SqlDbNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(SqlDbNotConfigured::new())
    }
}

impl Middleware for SqlDb {
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

/// Return whether a SQL statement should be executed through the row-returning path.
#[must_use]
pub fn statement_returns_rows(query: &str) -> bool {
    let Some(first_token) = query.split_whitespace().next().map(str::to_ascii_lowercase) else {
        return false;
    };

    matches!(
        first_token.as_str(),
        "select" | "with" | "show" | "describe" | "pragma" | "explain"
    )
}

#[cfg(not(target_arch = "wasm32"))]
impl SqlDatabase for crate::Db {
    async fn exec(&self, query: &str, params: &[SqlValue]) -> Result<SqlResult, SqlError> {
        use crate::sea_orm::{ConnectionTrait, FromQueryResult, JsonValue, Statement};

        let statement = Statement::from_sql_and_values(
            self.get_database_backend(),
            query,
            params.iter().cloned().map(sql_value_to_sea_value),
        );

        if statement_returns_rows(query) {
            let rows = self
                .query_all(statement)
                .await
                .map_err(|error| SqlError::Backend(error.to_string()))?;
            let rows: Result<Vec<_>, _> = rows
                .iter()
                .map(|row| {
                    JsonValue::from_query_result(row, "")
                        .map_err(|error| SqlError::Backend(error.to_string()))
                })
                .collect();
            let rows = rows?;
            return Ok(SqlResult {
                rows_read: rows.len() as u64,
                rows,
                rows_written: 0,
            });
        }

        let result = self
            .execute(statement)
            .await
            .map_err(|error| SqlError::Backend(error.to_string()))?;
        Ok(SqlResult {
            rows: Vec::new(),
            rows_read: 0,
            rows_written: result.rows_affected(),
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn sql_value_to_sea_value(value: SqlValue) -> crate::sea_orm::sea_query::Value {
    use crate::sea_orm::sea_query::Value;

    match value {
        SqlValue::Null => Value::String(None),
        SqlValue::Boolean(value) => Value::Bool(Some(value)),
        SqlValue::Integer(value) => Value::BigInt(Some(value)),
        SqlValue::Real(value) => Value::Double(Some(value)),
        SqlValue::Text(value) => Value::String(Some(Box::new(value))),
        SqlValue::Blob(value) => Value::Bytes(Some(Box::new(value))),
    }
}
