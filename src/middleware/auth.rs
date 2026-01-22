//! Authentication middleware.
//!
//! This module provides the [`AuthMiddleware`] for authenticating requests,
//! along with re-exports of authentication-related types.
//!
//! # Re-exports
//!
//! - [`BearerToken`]: Bearer token extractor (requires `auth` feature)
//! - [`JwtConfig`], [`JwtAuthenticator`], [`JwtError`]: JWT support (requires `jwt` feature, native only)
//! - [`Admin`], [`HasRoles`], [`AuthorizationError`]: Role-based guards (requires `auth` feature)

use std::future::Future;

use http_kit::{middleware::MiddlewareError, Endpoint, HttpError, Middleware, Request, Response};

use crate::utils::State;

// Re-export auth types for convenience
#[cfg(feature = "auth")]
pub use crate::extract::auth::BearerToken;

#[cfg(all(feature = "jwt", not(target_arch = "wasm32")))]
pub use crate::auth::jwt::{JwtAuthenticator, JwtConfig, JwtError};

#[cfg(feature = "auth")]
pub use crate::auth::guard::{Admin, AuthorizationError, HasRoles, RoleExtractor};

/// Trait for authenticating users from requests.
pub trait Authenticator {
    /// The type of user returned upon successful authentication.
    type User;
    /// The error type returned when authentication fails.
    type Error;

    /// Authenticate a user from the given request.
    fn authenticate(
        &self,
        req: &Request,
    ) -> impl Future<Output = Result<Self::User, Self::Error>> + Send;
}

/// Middleware for authenticating requests.
#[derive(Clone, Debug)]
pub struct AuthMiddleware<A: Authenticator> {
    authenticator: A,
}

impl<A: Authenticator> AuthMiddleware<A> {
    /// Create a new authentication middleware.
    pub const fn new(authenticator: A) -> Self {
        Self { authenticator }
    }
}

impl<A> Middleware for AuthMiddleware<A>
where
    A: Authenticator + Send + Sync + Clone + 'static,
    A::User: Send + Sync + Clone + 'static,
    A::Error: HttpError,
{
    type Error = A::Error;

    async fn handle<N: Endpoint>(
        &mut self,
        request: &mut Request,
        mut next: N,
    ) -> Result<Response, MiddlewareError<N::Error, Self::Error>> {
        match self.authenticator.authenticate(request).await {
            Ok(user) => {
                request.extensions_mut().insert(State(user));
                next.respond(request)
                    .await
                    .map_err(MiddlewareError::Endpoint)
            }
            Err(err) => Err(MiddlewareError::Middleware(err)),
        }
    }
}
