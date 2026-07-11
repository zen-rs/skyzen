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

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use http_kit::{http_error, middleware::MiddlewareError, Endpoint, HttpError};

    use super::{AuthMiddleware, Authenticator};
    use crate::{utils::State, Body, Middleware, Request, Response, StatusCode};

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

        async fn authenticate(&self, _req: &Request) -> Result<Self::User, Self::Error> {
            match &self.outcome {
                AuthOutcome::Allow(user) => Ok(user.clone()),
                AuthOutcome::Deny => Err(TestAuthError::new()),
            }
        }
    }

    #[derive(Debug)]
    struct CaptureUserEndpoint {
        calls: Arc<AtomicUsize>,
    }

    impl Endpoint for CaptureUserEndpoint {
        type Error = http_kit::error::BoxHttpError;

        async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let user = request
                .extensions()
                .get::<State<TestUser>>()
                .cloned()
                .expect("auth middleware should inject user state");
            Ok(Response::new(Body::from(user.name)))
        }
    }

    #[tokio::test]
    async fn successful_authentication_injects_user_for_next_endpoint() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut middleware =
            AuthMiddleware::new(TestAuthenticator::allow(TestUser { name: "lexo" }));
        let mut request = Request::new(Body::empty());

        let response = middleware
            .handle(
                &mut request,
                CaptureUserEndpoint {
                    calls: Arc::clone(&calls),
                },
            )
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "lexo");
    }

    #[tokio::test]
    async fn failed_authentication_short_circuits_endpoint_execution() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut middleware = AuthMiddleware::new(TestAuthenticator::deny());
        let mut request = Request::new(Body::empty());

        let error = middleware
            .handle(
                &mut request,
                CaptureUserEndpoint {
                    calls: Arc::clone(&calls),
                },
            )
            .await
            .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        match error {
            MiddlewareError::Middleware(error) => {
                assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
            }
            MiddlewareError::Endpoint(_) => {
                panic!("authentication failure should not reach endpoint")
            }
        }
    }
}
