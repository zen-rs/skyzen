use std::future::Future;

use http_kit::{middleware::MiddlewareError, Endpoint, HttpError, Middleware, Request, Response};

use crate::utils::State;

/// Trait for authenticating users from requests.
pub trait Authenticater {
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
#[derive(Clone)]
pub struct AuthMiddleware<A: Authenticater> {
    authenticator: A,
}

impl<A: Authenticater> AuthMiddleware<A> {
    /// Create a new authentication middleware.
    pub fn new(authenticator: A) -> Self {
        Self { authenticator }
    }
}

impl<A> Middleware for AuthMiddleware<A>
where
    A: Authenticater + Send + Sync + Clone + 'static,
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
