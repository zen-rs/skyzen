//! Role-based authorization guards.
//!
//! This module provides traits and extractors for role-based access control (RBAC).
//!
//! # Example
//!
//! ```rust,ignore
//! use skyzen::auth::guard::{Admin, HasRoles};
//!
//! #[derive(Clone)]
//! struct User {
//!     name: String,
//!     roles: Vec<String>,
//! }
//!
//! impl HasRoles for User {
//!     fn has_role(&self, role: &str) -> bool {
//!         self.roles.iter().any(|r| r == role)
//!     }
//! }
//!
//! // This handler requires admin role
//! async fn admin_only(Admin(user): Admin<User>) -> String {
//!     format!("Hello admin: {}", user.name)
//! }
//! ```

use http::StatusCode;

use crate::{extract::Extractor, utils::State, Request};

/// Trait for types that can provide role information.
///
/// Implement this trait for your user/claims type to enable role-based guards.
pub trait HasRoles {
    /// Check if the user has the specified role.
    fn has_role(&self, role: &str) -> bool;
}

/// Trait for role-based extractors to specify which role they require.
///
/// This is used by the guard extractors to determine which role to check.
pub trait RoleExtractor {
    /// The role string this extractor requires.
    const ROLE: &'static str;
}

/// Error returned when authorization fails.
#[skyzen::error(status = StatusCode::FORBIDDEN)]
pub enum AuthorizationError {
    /// The user is not authenticated (no user found in request extensions).
    #[error("User not authenticated", status = StatusCode::UNAUTHORIZED)]
    NotAuthenticated,
    /// The user does not have the required role.
    #[error("Insufficient permissions")]
    Forbidden,
}

/// Admin role guard extractor.
///
/// Extracts the user from request extensions and validates they have the "admin" role.
/// The user must have been previously injected by an authentication middleware.
///
/// # Type Parameter
///
/// - `U`: The user type that implements [`HasRoles`], [`Clone`], [`Send`], [`Sync`], and `'static`.
///
/// # Example
///
/// ```rust,ignore
/// use skyzen::auth::guard::{Admin, HasRoles};
///
/// #[derive(Clone)]
/// struct Claims {
///     sub: String,
///     roles: Vec<String>,
/// }
///
/// impl HasRoles for Claims {
///     fn has_role(&self, role: &str) -> bool {
///         self.roles.iter().any(|r| r == role)
///     }
/// }
///
/// async fn admin_dashboard(Admin(claims): Admin<Claims>) -> String {
///     format!("Welcome to admin dashboard, {}", claims.sub)
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Admin<U>(pub U);

impl_deref!(Admin);

impl<U> RoleExtractor for Admin<U> {
    const ROLE: &'static str = "admin";
}

impl<U> Extractor for Admin<U>
where
    U: HasRoles + Clone + Send + Sync + 'static,
{
    type Error = AuthorizationError;

    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let user = request
            .extensions()
            .get::<State<U>>()
            .ok_or(AuthorizationError::NotAuthenticated)?
            .0
            .clone();

        if user.has_role(Self::ROLE) {
            Ok(Self(user))
        } else {
            Err(AuthorizationError::Forbidden)
        }
    }
}

/// Macro to define custom role guard extractors.
///
/// This macro generates a newtype wrapper around your user type that checks for a specific role.
///
/// # Example
///
/// ```rust,ignore
/// use skyzen::define_role_guard;
///
/// // Define an Editor role guard
/// define_role_guard!(Editor, "editor", "Editor role guard");
///
/// // Use it in a handler
/// async fn edit_content(Editor(user): Editor<User>) -> String {
///     format!("Editing as: {}", user.name)
/// }
/// ```
#[macro_export]
macro_rules! define_role_guard {
    ($name:ident, $role:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone)]
        pub struct $name<U>(pub U);

        impl<U: Send + Sync> std::ops::Deref for $name<U> {
            type Target = U;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl<U: Send + Sync> std::ops::DerefMut for $name<U> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }

        impl<U> $crate::auth::guard::RoleExtractor for $name<U> {
            const ROLE: &'static str = $role;
        }

        impl<U> $crate::extract::Extractor for $name<U>
        where
            U: $crate::auth::guard::HasRoles + Clone + Send + Sync + 'static,
        {
            type Error = $crate::auth::guard::AuthorizationError;

            async fn extract(request: &mut $crate::Request) -> Result<Self, Self::Error> {
                use $crate::auth::guard::RoleExtractor;

                let user = request
                    .extensions()
                    .get::<$crate::utils::State<U>>()
                    .ok_or($crate::auth::guard::AuthorizationError::NotAuthenticated)?
                    .0
                    .clone();

                if user.has_role(Self::ROLE) {
                    Ok(Self(user))
                } else {
                    Err($crate::auth::guard::AuthorizationError::Forbidden)
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use skyzen_core::Extractor;

    use super::{Admin, AuthorizationError, HasRoles};
    use crate::utils::State;

    #[derive(Clone, Debug)]
    struct TestUser {
        name: String,
        roles: Vec<String>,
    }

    impl HasRoles for TestUser {
        fn has_role(&self, role: &str) -> bool {
            self.roles.iter().any(|r| r == role)
        }
    }

    #[tokio::test]
    async fn test_admin_guard_success() {
        let user = TestUser {
            name: "Alice".to_owned(),
            roles: vec!["admin".to_owned(), "user".to_owned()],
        };

        let mut request = http::Request::builder()
            .body(http_kit::Body::empty())
            .unwrap();

        request.extensions_mut().insert(State(user.clone()));

        let result = Admin::<TestUser>::extract(&mut request).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().0.name, "Alice");
    }

    #[tokio::test]
    async fn test_admin_guard_forbidden() {
        let user = TestUser {
            name: "Bob".to_owned(),
            roles: vec!["user".to_owned()],
        };

        let mut request = http::Request::builder()
            .body(http_kit::Body::empty())
            .unwrap();

        request.extensions_mut().insert(State(user));

        let result = Admin::<TestUser>::extract(&mut request).await;
        assert!(matches!(result, Err(AuthorizationError::Forbidden)));
    }

    #[tokio::test]
    async fn test_admin_guard_not_authenticated() {
        let mut request = http::Request::builder()
            .body(http_kit::Body::empty())
            .unwrap();

        let result = Admin::<TestUser>::extract(&mut request).await;
        assert!(matches!(result, Err(AuthorizationError::NotAuthenticated)));
    }
}
