use std::future::Future;

use http_kit::Request;

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
pub struct AuthMiddleware<A: Authenticater> {
    authenticator: A,
}
