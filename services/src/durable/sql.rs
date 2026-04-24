//! Durable Object database abstraction.

use core::{convert::Infallible, future::Future};

use http_kit::{
    http_error,
    middleware::{Middleware, MiddlewareError},
    Endpoint, Response,
};
use serde::de::DeserializeOwned;
use skyzen_core::{Extractor, StatusCode};

use crate::{
    maybe_send::{BoxFuture, MaybeSend},
    sql::{DbError, DbExecResult, DbValue},
};

/// Errors from Durable Object database operations.
#[derive(Debug, thiserror::Error)]
pub enum DurableDbError {
    /// The underlying storage backend returned an error.
    #[error("durable database error: {0}")]
    Backend(String),

    /// Serialization or deserialization failed.
    #[error("durable database serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A query expected one row but none were returned.
    #[error("durable database row not found")]
    RowNotFound,
}

impl From<DbError> for DurableDbError {
    fn from(error: DbError) -> Self {
        Self::Backend(error.to_string())
    }
}

/// Durable Object SQL storage.
pub trait DurableDbBackend: Send + Sync + Clone + 'static {
    /// Execute a query that returns rows.
    fn query(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + MaybeSend;

    /// Execute a statement that does not return rows.
    fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> impl Future<Output = Result<DbExecResult, DurableDbError>> + MaybeSend;

    /// Get the on-disk database size in bytes.
    fn database_size(&self) -> impl Future<Output = Result<u64, DurableDbError>> + MaybeSend;
}

trait DurableDbBackendObj: Send + Sync {
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>>;
    fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>>;
    fn database_size(&self) -> BoxFuture<'_, Result<u64, DurableDbError>>;
    fn clone_box(&self) -> Box<dyn DurableDbBackendObj>;
}

impl<T: DurableDbBackend> DurableDbBackendObj for T {
    fn query<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>> {
        Box::pin(DurableDbBackend::query(self, query, params))
    }

    fn execute<'a>(
        &'a self,
        query: &'a str,
        params: &'a [DbValue],
    ) -> BoxFuture<'a, Result<DbExecResult, DurableDbError>> {
        Box::pin(DurableDbBackend::execute(self, query, params))
    }

    fn database_size(&self) -> BoxFuture<'_, Result<u64, DurableDbError>> {
        Box::pin(DurableDbBackend::database_size(self))
    }

    fn clone_box(&self) -> Box<dyn DurableDbBackendObj> {
        Box::new(self.clone())
    }
}

/// Type-erased Durable Object database extractor.
pub struct DurableDb(Box<dyn DurableDbBackendObj>);

impl Clone for DurableDb {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl std::fmt::Debug for DurableDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableDb").finish_non_exhaustive()
    }
}

impl DurableDb {
    /// Create a new `DurableDb` from any [`DurableDbBackend`] implementation.
    pub fn new(store: impl DurableDbBackend) -> Self {
        Self(Box::new(store))
    }

    /// Start building a query against the Durable Object database.
    #[must_use]
    pub const fn query<'a>(&'a self, sql: &'a str) -> DurableDbQuery<'a> {
        DurableDbQuery {
            db: self,
            sql,
            params: Vec::new(),
        }
    }

    /// Get the on-disk database size in bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot report its database size.
    pub async fn database_size(&self) -> Result<u64, DurableDbError> {
        self.0.database_size().await
    }
}

/// Query builder for [`DurableDb`].
#[derive(Debug)]
pub struct DurableDbQuery<'a> {
    db: &'a DurableDb,
    sql: &'a str,
    params: Vec<DbValue>,
}

impl DurableDbQuery<'_> {
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
    /// Returns an error if backend execution or result conversion fails.
    pub async fn execute(self) -> Result<DbExecResult, DurableDbError> {
        self.db.0.execute(self.sql, &self.params).await
    }

    /// Execute a query and deserialize all rows into `T`.
    ///
    /// # Errors
    ///
    /// Returns an error if backend execution or row deserialization fails.
    pub async fn fetch_all<T>(self) -> Result<Vec<T>, DurableDbError>
    where
        T: DeserializeOwned,
    {
        let result = self.db.0.query(self.sql, &self.params).await?;
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
    /// Returns an error if backend execution or row deserialization fails.
    pub async fn fetch_optional<T>(self) -> Result<Option<T>, DurableDbError>
    where
        T: DeserializeOwned,
    {
        let result = self.db.0.query(self.sql, &self.params).await?;
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
    pub async fn fetch_one<T>(self) -> Result<T, DurableDbError>
    where
        T: DeserializeOwned,
    {
        self.fetch_optional()
            .await?
            .ok_or(DurableDbError::RowNotFound)
    }
}

http_error!(
    /// The Durable Object database was not found in request extensions.
    pub DurableDbNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Durable database not configured. Ensure a DurableDbBackend implementation is injected."
);

impl Extractor for DurableDb {
    type Error = DurableDbNotConfigured;

    async fn extract(request: &mut http_kit::Request) -> Result<Self, Self::Error> {
        request
            .extensions()
            .get::<Self>()
            .cloned()
            .ok_or(DurableDbNotConfigured::new())
    }
}

impl Middleware for DurableDb {
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
