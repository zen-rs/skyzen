//! Authentication and authorization utilities.
//!
//! This module provides tools for handling authentication and authorization in Skyzen:
//!
//! - [`guard`]: Role-based authorization guards for protecting routes
//! - [`jwt`]: JWT token verification and authentication (requires `jwt` feature, native only)
//!
//! # Example
//!
//! ```rust,ignore
//! use skyzen::auth::guard::{Admin, HasRoles};
//! use skyzen::auth::jwt::{JwtConfig, JwtAuthenticator};
//! use skyzen::middleware::auth::AuthMiddleware;
//!
//! #[derive(Clone, serde::Deserialize)]
//! struct Claims {
//!     sub: String,
//!     roles: Vec<String>,
//! }
//!
//! impl HasRoles for Claims {
//!     fn has_role(&self, role: &str) -> bool {
//!         self.roles.iter().any(|r| r == role)
//!     }
//! }
//!
//! // Set up JWT authentication middleware
//! let jwt_config = JwtConfig::with_secret(b"my-secret-key");
//! let auth = AuthMiddleware::new(JwtAuthenticator::<Claims>::new(jwt_config));
//!
//! // Protected admin route
//! async fn admin_only(Admin(claims): Admin<Claims>) -> String {
//!     format!("Hello admin: {}", claims.sub)
//! }
//! ```

pub mod guard;

#[cfg(all(feature = "jwt", not(target_arch = "wasm32")))]
pub mod jwt;

pub use guard::{Admin, AuthorizationError, HasRoles, RoleExtractor};

#[cfg(all(feature = "jwt", not(target_arch = "wasm32")))]
pub use jwt::{JwtAuthenticator, JwtConfig, JwtError};
