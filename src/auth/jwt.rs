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

use core::future::{ready, Future};
use std::marker::PhantomData;

use http::StatusCode;
use jsonwebtoken::{decode, Algorithm, DecodingKey, TokenData, Validation};
use serde::de::DeserializeOwned;

use crate::{
    extract::auth::parse_bearer, extract::auth::BearerTokenError, middleware::auth::Authenticator,
    Request,
};

/// Cryptographic family of the configured decoding key.
///
/// Tracking this lets [`JwtConfig::with_algorithms`] reject algorithm/key combinations that would
/// otherwise enable algorithm-confusion attacks (e.g. accepting an `HS256` token verified against
/// an RSA public key used as an HMAC secret).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyFamily {
    Hmac,
    Rsa,
    Ec,
}

impl KeyFamily {
    /// Returns whether `algorithm` belongs to this key's family.
    const fn permits(self, algorithm: Algorithm) -> bool {
        match self {
            Self::Hmac => matches!(
                algorithm,
                Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
            ),
            Self::Rsa => matches!(
                algorithm,
                Algorithm::RS256
                    | Algorithm::RS384
                    | Algorithm::RS512
                    | Algorithm::PS256
                    | Algorithm::PS384
                    | Algorithm::PS512
            ),
            Self::Ec => matches!(algorithm, Algorithm::ES256 | Algorithm::ES384),
        }
    }
}

/// Configuration for JWT verification.
///
/// Holds the decoding key and validation settings used to verify JWT tokens.
///
/// # Security
///
/// By default neither the issuer nor the audience claim is required (matching `jsonwebtoken`'s
/// defaults). For production deployments you should pin the expected issuer/audience via
/// [`with_issuer`](Self::with_issuer) / [`with_audience`](Self::with_audience). The permitted
/// signing algorithms are fixed to the key's cryptographic family at construction time and cannot
/// be widened across families.
#[derive(Clone)]
pub struct JwtConfig {
    decoding_key: DecodingKey,
    validation: Validation,
    key_family: KeyFamily,
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
            key_family: KeyFamily::Hmac,
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
                v.algorithms = vec![Algorithm::RS256];
                v
            },
            key_family: KeyFamily::Rsa,
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
                v.algorithms = vec![Algorithm::ES256];
                v
            },
            key_family: KeyFamily::Ec,
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

    /// Restrict verification to the given set of allowed algorithms.
    ///
    /// All algorithms must belong to the same cryptographic family as the configured key. This
    /// prevents algorithm-confusion attacks where, for example, an `HS256` token is forged using an
    /// RSA public key as the HMAC secret.
    ///
    /// # Panics
    ///
    /// Panics if any algorithm is incompatible with the key's family — a configuration error caught
    /// at startup rather than at request time.
    #[must_use]
    pub fn with_algorithms(mut self, algorithms: Vec<Algorithm>) -> Self {
        for algorithm in &algorithms {
            assert!(
                self.key_family.permits(*algorithm),
                "JWT algorithm {algorithm:?} is incompatible with the configured {:?} key",
                self.key_family
            );
        }
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
    /// The Authorization header is missing, malformed, or not a Bearer credential.
    #[error("Invalid Authorization header")]
    Header(#[from] BearerTokenError),
    /// The JWT signature is invalid.
    #[error("Invalid token signature")]
    InvalidSignature,
    /// The token's signing algorithm is not accepted (possible algorithm-confusion attempt).
    #[error("Unsupported or disallowed token algorithm")]
    InvalidAlgorithm,
    /// The JWT has expired.
    #[error("Token has expired")]
    Expired,
    /// The JWT is not yet valid (`nbf` in the future).
    #[error("Token is not yet valid")]
    NotYetValid,
    /// The JWT claims are invalid (issuer, audience, missing required claim, etc.).
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
            ErrorKind::ImmatureSignature => Self::NotYetValid,
            ErrorKind::InvalidIssuer
            | ErrorKind::InvalidAudience
            | ErrorKind::InvalidSubject
            | ErrorKind::MissingRequiredClaim(_) => Self::InvalidClaims,
            ErrorKind::InvalidAlgorithm
            | ErrorKind::InvalidAlgorithmName
            | ErrorKind::MissingAlgorithm => Self::InvalidAlgorithm,
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

    // Decoding a JWT is pure computation over the request's own header, so the future is ready
    // on creation rather than an `async` block with nothing to await.
    fn authenticate(
        &self,
        req: &Request,
    ) -> impl Future<Output = Result<Self::User, Self::Error>> + Send {
        ready(
            parse_bearer(req)
                .map_err(Self::Error::from)
                .and_then(|token| Ok(self.config.decode::<C>(token)?.claims)),
        )
    }
}

#[cfg(test)]
mod tests {
    use http::header::AUTHORIZATION;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::{Deserialize, Serialize};

    use super::{JwtAuthenticator, JwtConfig, JwtError};
    use crate::extract::auth::BearerTokenError;
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
        assert!(matches!(
            result,
            Err(JwtError::Header(BearerTokenError::MissingHeader))
        ));
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
        assert!(matches!(
            result,
            Err(JwtError::Header(BearerTokenError::NotBearer))
        ));
    }

    #[tokio::test]
    async fn test_jwt_accepts_case_insensitive_scheme() {
        let secret = b"test-secret-key";
        let claims = TestClaims {
            sub: "user123".to_owned(),
            exp: u64::MAX,
            roles: vec![],
        };
        let token = create_token(&claims, secret);
        let authenticator = JwtAuthenticator::<TestClaims>::new(JwtConfig::with_secret(secret));

        // Lowercase scheme and extra whitespace must still be accepted (RFC 7235).
        let request = http::Request::builder()
            .header(AUTHORIZATION, format!("bearer   {token}"))
            .body(http_kit::Body::empty())
            .unwrap();

        assert_eq!(authenticator.authenticate(&request).await.unwrap(), claims);
    }

    #[test]
    #[should_panic(expected = "incompatible with the configured Hmac key")]
    fn test_with_algorithms_rejects_cross_family() {
        // Adding an RSA algorithm to an HMAC key must fail loudly at configuration time.
        let _ =
            JwtConfig::with_secret(b"secret").with_algorithms(vec![jsonwebtoken::Algorithm::RS256]);
    }
}
