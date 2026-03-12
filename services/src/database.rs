//! Database abstraction using `SeaORM`.
//!
//! The [`Db`] extractor wraps a [`sea_orm::DatabaseConnection`], providing
//! a thin extraction layer while delegating all ORM functionality to `SeaORM`.

use std::{
    convert::Infallible,
    ops::{Deref, DerefMut},
};

use http_kit::{
    http_error,
    middleware::{Middleware, MiddlewareError},
    Endpoint, Response,
};
use skyzen_core::{Extractor, StatusCode};

/// A database connection extractor wrapping [`sea_orm::DatabaseConnection`].
///
/// `Db` is a thin wrapper that integrates `SeaORM`'s `DatabaseConnection` with
/// Skyzen's extractor system. It implements `Deref` to `DatabaseConnection`,
/// so you can use it directly with `SeaORM`'s query API.
///
/// # Example
///
/// ```ignore
/// use skyzen_services::{Db, sea_orm::*};
/// use entity::user;
///
/// async fn handler(db: Db) -> Result<Json<Vec<user::Model>>> {
///     let users = user::Entity::find().all(&*db).await?;
///     Ok(Json(users))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Db(sea_orm::DatabaseConnection);

impl Db {
    /// Create a new `Db` from a `SeaORM` database connection.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // DatabaseConnection has a non-trivial destructor
    pub fn new(conn: sea_orm::DatabaseConnection) -> Self {
        Self(conn)
    }

    /// Get a reference to the underlying `DatabaseConnection`.
    #[must_use]
    pub const fn inner(&self) -> &sea_orm::DatabaseConnection {
        &self.0
    }

    /// Consume the wrapper and return the underlying `DatabaseConnection`.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // DatabaseConnection has a non-trivial destructor
    pub fn into_inner(self) -> sea_orm::DatabaseConnection {
        self.0
    }
}

impl Deref for Db {
    type Target = sea_orm::DatabaseConnection;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Db {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

http_error!(
    /// The database service was not found in request extensions.
    pub DbNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Database not configured. Ensure a Db instance is injected via middleware."
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
