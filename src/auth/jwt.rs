//! JWT (JSON Web Token) authentication.
//!
//! This module provides JWT verification and authentication via the [`JwtAuthenticator`] type.
//!
//! # Example
//!
//! ```rust,ignore
//! use skyzen::auth::jwt::{JwtConfig, JwtAuthenticator};
//! use skyzen::middleware::auth::AuthMiddleware;
//! use serde::Deserialize;
//!
//! #[derive(Clone, Deserialize)]
//! struct Claims {
//!     sub: String,
//!     exp: u64,
//!     roles: Vec<String>,
//! }
//!
//! let config = JwtConfig::with_secret(b"my-secret-key");
//! let authenticator = JwtAuthenticator::<Claims>::new(config);
//! let middleware = AuthMiddleware::new(authenticator);
//! ```

use std::marker::PhantomData;

use http::StatusCode;
use jsonwebtoken::{decode, DecodingKey, TokenData, Validation};
use serde::de::DeserializeOwned;

use crate::{header, middleware::auth::Authenticator, Request};

/// Configuration for JWT verification.
///
/// Holds the decoding key and validation settings used to verify JWT tokens.
#[derive(Clone)]
pub struct JwtConfig {
    decoding_key: DecodingKey,
    validation: Validation,
}

impl std::fmt::Debug for JwtConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtConfig")
            .field("validation", &self.validation)
            .finish_non_exhaustive()
    }
}

impl JwtConfig {
    /// Create a new JWT configuration with a secret key for HMAC algorithms.
    ///
    /// Uses HS256 algorithm by default.
    #[must_use]
    pub fn with_secret(secret: &[u8]) -> Self {
        Self {
            decoding_key: DecodingKey::from_secret(secret),
            validation: Validation::default(),
        }
    }

    /// Create a new JWT configuration with an RSA public key in PEM format.
    ///
    /// # Errors
    ///
    /// Returns an error if the PEM data is invalid.
    pub fn with_rsa_pem(pem: &[u8]) -> Result<Self, jsonwebtoken::errors::Error> {
        Ok(Self {
            decoding_key: DecodingKey::from_rsa_pem(pem)?,
            validation: {
                let mut v = Validation::default();
                v.algorithms = vec![jsonwebtoken::Algorithm::RS256];
                v
            },
        })
    }

    /// Create a new JWT configuration with an EC public key in PEM format.
    ///
    /// # Errors
    ///
    /// Returns an error if the PEM data is invalid.
    pub fn with_ec_pem(pem: &[u8]) -> Result<Self, jsonwebtoken::errors::Error> {
        Ok(Self {
            decoding_key: DecodingKey::from_ec_pem(pem)?,
            validation: {
                let mut v = Validation::default();
                v.algorithms = vec![jsonwebtoken::Algorithm::ES256];
                v
            },
        })
    }

    /// Set the expected issuer claim.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.validation.set_issuer(&[issuer.into()]);
        self
    }

    /// Set the expected audience claim.
    #[must_use]
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.validation.set_audience(&[audience.into()]);
        self
    }

    /// Set multiple allowed algorithms.
    #[must_use]
    pub fn with_algorithms(mut self, algorithms: Vec<jsonwebtoken::Algorithm>) -> Self {
        self.validation.algorithms = algorithms;
        self
    }

    /// Disable expiration validation.
    ///
    /// # Warning
    ///
    /// Only use this for testing or when you handle expiration validation yourself.
    #[must_use]
    pub const fn without_expiration_validation(mut self) -> Self {
        self.validation.validate_exp = false;
        self
    }

    /// Set the leeway (in seconds) for time-based claims (exp, nbf).
    #[must_use]
    pub const fn with_leeway(mut self, leeway: u64) -> Self {
        self.validation.leeway = leeway;
        self
    }

    /// Decode a JWT token using this configuration.
    fn decode<C: DeserializeOwned>(
        &self,
        token: &str,
    ) -> Result<TokenData<C>, jsonwebtoken::errors::Error> {
        decode::<C>(token, &self.decoding_key, &self.validation)
    }
}

/// Error returned when JWT authentication fails.
#[skyzen::error(status = StatusCode::UNAUTHORIZED)]
pub enum JwtError {
    /// The Authorization header is missing.
    #[error("Missing Authorization header")]
    MissingHeader,
    /// The Authorization header is not valid UTF-8.
    #[error("Invalid Authorization header encoding")]
    InvalidEncoding,
    /// The Authorization header does not use the Bearer scheme.
    #[error("Authorization header must use Bearer scheme")]
    NotBearer,
    /// The JWT signature is invalid.
    #[error("Invalid token signature")]
    InvalidSignature,
    /// The JWT has expired.
    #[error("Token has expired")]
    Expired,
    /// The JWT claims are invalid (issuer, audience, etc.).
    #[error("Invalid token claims")]
    InvalidClaims,
    /// The JWT is malformed and cannot be parsed.
    #[error("Malformed token")]
    Malformed,
}

impl From<jsonwebtoken::errors::Error> for JwtError {
    fn from(err: jsonwebtoken::errors::Error) -> Self {
        use jsonwebtoken::errors::ErrorKind;
        match err.kind() {
            ErrorKind::InvalidSignature => Self::InvalidSignature,
            ErrorKind::ExpiredSignature => Self::Expired,
            ErrorKind::InvalidIssuer
            | ErrorKind::InvalidAudience
            | ErrorKind::ImmatureSignature => Self::InvalidClaims,
            _ => Self::Malformed,
        }
    }
}

/// JWT authenticator that implements the [`Authenticator`] trait.
///
/// Extracts the Bearer token from the Authorization header, verifies it,
/// and decodes the claims into the specified type.
///
/// # Type Parameter
///
/// - `C`: The claims type that the JWT payload will be deserialized into.
///   Must implement `DeserializeOwned`, `Clone`, `Send`, and `Sync`.
#[derive(Clone)]
pub struct JwtAuthenticator<C> {
    config: JwtConfig,
    _claims: PhantomData<fn() -> C>,
}

impl<C> std::fmt::Debug for JwtAuthenticator<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtAuthenticator")
            .field("config", &self.config)
            .finish()
    }
}

impl<C> JwtAuthenticator<C> {
    /// Create a new JWT authenticator with the given configuration.
    #[must_use]
    pub const fn new(config: JwtConfig) -> Self {
        Self {
            config,
            _claims: PhantomData,
        }
    }
}

impl<C> Authenticator for JwtAuthenticator<C>
where
    C: DeserializeOwned + Clone + Send + Sync + 'static,
{
    type User = C;
    type Error = JwtError;

    async fn authenticate(&self, req: &Request) -> Result<Self::User, Self::Error> {
        let header_value = req
            .headers()
            .get(header::AUTHORIZATION)
            .ok_or(JwtError::MissingHeader)?;

        let value = header_value
            .to_str()
            .map_err(|_| JwtError::InvalidEncoding)?;

        let token = value.strip_prefix("Bearer ").ok_or(JwtError::NotBearer)?;

        let token_data = self.config.decode::<C>(token)?;
        Ok(token_data.claims)
    }
}

#[cfg(test)]
mod tests {
    use http::header::AUTHORIZATION;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::{Deserialize, Serialize};

    use super::{JwtAuthenticator, JwtConfig, JwtError};
    use crate::middleware::auth::Authenticator;

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct TestClaims {
        sub: String,
        exp: u64,
        roles: Vec<String>,
    }

    fn create_token(claims: &TestClaims, secret: &[u8]) -> String {
        encode(
            &Header::default(),
            claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_jwt_authentication_success() {
        let secret = b"test-secret-key";
        let claims = TestClaims {
            sub: "user123".to_owned(),
            exp: u64::MAX, // Far future
            roles: vec!["user".to_owned()],
        };

        let token = create_token(&claims, secret);
        let config = JwtConfig::with_secret(secret);
        let authenticator = JwtAuthenticator::<TestClaims>::new(config);

        let request = http::Request::builder()
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(http_kit::Body::empty())
            .unwrap();

        let result = authenticator.authenticate(&request).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), claims);
    }

    #[tokio::test]
    async fn test_jwt_missing_header() {
        let config = JwtConfig::with_secret(b"secret");
        let authenticator = JwtAuthenticator::<TestClaims>::new(config);

        let request = http::Request::builder()
            .body(http_kit::Body::empty())
            .unwrap();

        let result = authenticator.authenticate(&request).await;
        assert!(matches!(result, Err(JwtError::MissingHeader)));
    }

    #[tokio::test]
    async fn test_jwt_invalid_signature() {
        let claims = TestClaims {
            sub: "user123".to_owned(),
            exp: u64::MAX,
            roles: vec![],
        };

        // Create token with one secret, verify with another
        let token = create_token(&claims, b"secret1");
        let config = JwtConfig::with_secret(b"secret2");
        let authenticator = JwtAuthenticator::<TestClaims>::new(config);

        let request = http::Request::builder()
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(http_kit::Body::empty())
            .unwrap();

        let result = authenticator.authenticate(&request).await;
        assert!(matches!(result, Err(JwtError::InvalidSignature)));
    }

    #[tokio::test]
    async fn test_jwt_expired() {
        let secret = b"secret";
        let claims = TestClaims {
            sub: "user123".to_owned(),
            exp: 0, // Already expired
            roles: vec![],
        };

        let token = create_token(&claims, secret);
        let config = JwtConfig::with_secret(secret);
        let authenticator = JwtAuthenticator::<TestClaims>::new(config);

        let request = http::Request::builder()
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(http_kit::Body::empty())
            .unwrap();

        let result = authenticator.authenticate(&request).await;
        assert!(matches!(result, Err(JwtError::Expired)));
    }

    #[tokio::test]
    async fn test_jwt_not_bearer() {
        let config = JwtConfig::with_secret(b"secret");
        let authenticator = JwtAuthenticator::<TestClaims>::new(config);

        let request = http::Request::builder()
            .header(AUTHORIZATION, "Basic dXNlcjpwYXNz")
            .body(http_kit::Body::empty())
            .unwrap();

        let result = authenticator.authenticate(&request).await;
        assert!(matches!(result, Err(JwtError::NotBearer)));
    }
}
