//! Authentication and authorization utilities.
//!
//! This module provides tools for handling authentication and authorization in Skyzen across both
//! native servers and `wasm32` edge targets:
//!
//! - [`guard`]: Role-based authorization guards for protecting routes
//! - [`jwt`]: JWT token verification and authentication (requires `jwt` feature)
//! - [`jwks`]: JWKS (RFC 7517) parsing, verification, and cached remote key rotation (requires `jwt` feature; remote fetching requires `oidc`)
//! - [`session`]: Session-backed authentication middleware feeding [`AuthUser`](crate::middleware::auth::AuthUser) and role guards
//! - [`oidc`]: OAuth 2.0 / OIDC PKCE, discovery, token exchange, and ID token verification helpers
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
pub mod oidc;
pub mod session;

#[cfg(feature = "jwt")]
pub mod jwks;
#[cfg(feature = "jwt")]
pub mod jwt;
#[cfg(all(feature = "jwt", target_arch = "wasm32"))]
mod wasm_jwt;

pub use guard::{Admin, AuthorizationError, HasRoles, RoleExtractor};
pub use oidc::{
    generate_nonce, generate_state, AuthorizationRequest, OidcError, OidcProviderMetadata,
    PkceChallenge, PkceVerifier, TokenExchangeRequest, TokenResponse,
};
pub use session::SessionAuthenticator;

#[cfg(feature = "oidc")]
pub use jwks::RemoteJwks;
#[cfg(feature = "jwt")]
pub use jwks::{JwksAuthenticator, JwksError, JwksValidation, JwtKeySet};
#[cfg(feature = "jwt")]
pub use jwt::{Algorithm, JwtAuthenticator, JwtConfig, JwtError};
#[cfg(feature = "oidc")]
pub use oidc::exchange_code;
#[cfg(feature = "jwt")]
pub use oidc::{verify_id_token, IdTokenClaims};
