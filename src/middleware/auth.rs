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

use std::{
    any::TypeId,
    future::{ready, Future},
    ops::{Deref, DerefMut},
};

use http_kit::{HttpError, Request, Response};
use skyzen_core::{
    middleware::{Middleware, Next},
    Error, Extractor, Requirement,
};

// Re-export auth types for convenience
#[cfg(feature = "auth")]
pub use crate::extract::auth::BearerToken;

#[cfg(all(feature = "jwt", not(target_arch = "wasm32")))]
pub use crate::auth::jwt::{JwtAuthenticator, JwtConfig, JwtError};

#[cfg(feature = "auth")]
pub use crate::auth::guard::{Admin, AuthorizationError, HasRoles, RoleExtractor};

/// The identity [`AuthMiddleware`] established for the current request.
///
/// This is deliberately a separate extensions slot from
/// [`State<T>`](crate::utils::State): an application whose shared state happens to have the same
/// type as its user or claims type would otherwise have the two overwrite each other.
#[derive(Debug, Clone)]
pub struct AuthUser<U>(pub U);

impl<U> Deref for AuthUser<U> {
    type Target = U;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<U> DerefMut for AuthUser<U> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

http_kit::http_error!(
    /// Raised when a handler asks for the authenticated user but no authenticator ran.
    pub NotAuthenticated,
    http_kit::StatusCode::UNAUTHORIZED,
    "Not authenticated."
);

impl<U: Send + Sync + Clone + 'static> Extractor for AuthUser<U> {
    type Error = NotAuthenticated;

    // Reading the user back out of the extensions is a synchronous clone, so the future is ready
    // on creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or_else(NotAuthenticated::new),
        )
    }

    fn requirements() -> Vec<Requirement> {
        vec![Requirement::of::<Self>(
            "`.with(AuthMiddleware::new(authenticator))`",
        )]
    }
}

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
    A: Authenticator + Send + Sync + 'static,
    A::User: Send + Sync + Clone + 'static,
    A::Error: HttpError,
{
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let user = self.authenticator.authenticate(request).await?;
        request.extensions_mut().insert(AuthUser(user));
        next.run(request).await
    }

    fn provisions(&self) -> Vec<TypeId> {
        vec![TypeId::of::<AuthUser<A::User>>()]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use core::future::{ready, Future};
    use http_kit::{http_error, HttpError};

    use super::{AuthMiddleware, AuthUser, Authenticator};
    use crate::{
        routing::{CreateRouteNode, Route},
        Body, Request, Result, StatusCode,
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestUser {
        name: &'static str,
    }

    http_error!(
        pub TestAuthError,
        StatusCode::UNAUTHORIZED,
        "authentication failed"
    );

    #[derive(Clone, Debug)]
    enum AuthOutcome {
        Allow(TestUser),
        Deny,
    }

    #[derive(Clone, Debug)]
    struct TestAuthenticator {
        outcome: AuthOutcome,
    }

    impl TestAuthenticator {
        const fn allow(user: TestUser) -> Self {
            Self {
                outcome: AuthOutcome::Allow(user),
            }
        }

        const fn deny() -> Self {
            Self {
                outcome: AuthOutcome::Deny,
            }
        }
    }

    impl Authenticator for TestAuthenticator {
        type User = TestUser;
        type Error = TestAuthError;

        fn authenticate(
            &self,
            _req: &Request,
        ) -> impl Future<Output = std::result::Result<Self::User, Self::Error>> + Send {
            ready(match &self.outcome {
                AuthOutcome::Allow(user) => Ok(user.clone()),
                AuthOutcome::Deny => Err(TestAuthError::new()),
            })
        }
    }

    /// Counts how often the endpoint ran, so a denied request can be shown never to reach it.
    #[derive(Clone, Debug)]
    struct Calls(Arc<AtomicUsize>);

    fn get(path: &str) -> Request {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = path.parse().expect("valid path");
        request
    }

    fn router(authenticator: TestAuthenticator, calls: &Calls) -> crate::routing::Router {
        let calls = calls.clone();
        Route::new(("/me".at(move |AuthUser(user): AuthUser<TestUser>| {
            let calls = calls.clone();
            async move {
                calls.0.fetch_add(1, Ordering::SeqCst);
                Result::Ok(user.name)
            }
        }),))
        .with(AuthMiddleware::new(authenticator))
        .build()
    }

    #[tokio::test]
    async fn successful_authentication_injects_the_user_for_the_endpoint() {
        let calls = Calls(Arc::new(AtomicUsize::new(0)));
        let router = router(TestAuthenticator::allow(TestUser { name: "lexo" }), &calls);

        let response = router.go(get("/me")).await.unwrap();

        assert_eq!(calls.0.load(Ordering::SeqCst), 1);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "lexo");
    }

    #[tokio::test]
    async fn failed_authentication_short_circuits_endpoint_execution() {
        let calls = Calls(Arc::new(AtomicUsize::new(0)));
        let router = router(TestAuthenticator::deny(), &calls);

        let error = router.go(get("/me")).await.unwrap_err();

        assert_eq!(calls.0.load(Ordering::SeqCst), 0);
        assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn an_unauthenticated_route_is_rejected_at_build_time() {
        let error = Route::new((
            "/me".at(|AuthUser(user): AuthUser<TestUser>| async move { Result::Ok(user.name) }),
        ))
        .try_build()
        .unwrap_err();

        assert!(
            error.to_string().contains("AuthMiddleware"),
            "the error should name the fix: {error}"
        );
    }

    #[test]
    fn application_state_and_the_authenticated_user_no_longer_share_a_slot() {
        use crate::utils::State;

        // Both the state and the user are `TestUser`; before `AuthUser` they collided.
        Route::new(("/me".at(
            |AuthUser(user): AuthUser<TestUser>, State(config): State<TestUser>| async move {
                Result::Ok(format!("{}/{}", user.name, config.name))
            },
        ),))
        .with(AuthMiddleware::new(TestAuthenticator::allow(TestUser {
            name: "lexo",
        })))
        .with(State(TestUser { name: "config" }))
        .try_build()
        .expect("distinct slots are both provided");
    }
}
