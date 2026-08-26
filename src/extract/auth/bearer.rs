//! Bearer token extraction from Authorization header.

use core::future::{ready, Future};
use http::StatusCode;

use crate::{extract::Extractor, header, Request};

/// Bearer token extracted from the Authorization header.
///
/// # Example
///
/// ```rust,ignore
/// use skyzen::extract::BearerToken;
///
/// async fn handler(BearerToken(token): BearerToken) -> String {
///     format!("Token: {token}")
/// }
/// ```
#[derive(Debug, Clone)]
pub struct BearerToken(pub String);

impl_deref!(BearerToken, String);

/// Error returned when extracting a Bearer token fails.
#[skyzen::error(status = StatusCode::UNAUTHORIZED)]
pub enum BearerTokenError {
    /// The Authorization header is missing from the request.
    #[error("Missing Authorization header")]
    MissingHeader,
    /// The Authorization header value is not valid UTF-8.
    #[error("Invalid Authorization header encoding")]
    InvalidEncoding,
    /// The Authorization header does not use the Bearer scheme.
    #[error("Authorization header must use Bearer scheme")]
    NotBearer,
}

/// Parse a Bearer token from the request's `Authorization` header.
///
/// The auth scheme is matched case-insensitively (`Bearer`/`bearer`/`BEARER`) per RFC 7235, and
/// any amount of whitespace between the scheme and the token is tolerated. The returned token
/// borrows from the request, so callers that need ownership should clone it.
///
/// # Errors
///
/// Returns [`BearerTokenError`] if the header is missing, not valid UTF-8, or does not carry a
/// non-empty Bearer credential.
// Re-exported crate-wide via `extract::auth::parse_bearer`, so `pub(crate)` is required despite the
// enclosing module being private.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn parse_bearer(request: &Request) -> Result<&str, BearerTokenError> {
    let value = request
        .headers()
        .get(header::AUTHORIZATION)
        .ok_or(BearerTokenError::MissingHeader)?
        .to_str()
        .map_err(|_| BearerTokenError::InvalidEncoding)?;

    let (scheme, token) = value
        .split_once(char::is_whitespace)
        .ok_or(BearerTokenError::NotBearer)?;

    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err(BearerTokenError::NotBearer);
    }

    let token = token.trim_start();
    if token.is_empty() {
        return Err(BearerTokenError::NotBearer);
    }

    Ok(token)
}

impl Extractor for BearerToken {
    type Error = BearerTokenError;

    // The header is already on the request, so the future is ready on creation rather than an
    // `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(parse_bearer(request).map(|token| Self(token.to_owned())))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            location: crate::openapi::ParameterLocation::Header,
            content_type: None,
            schema: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use http::header::AUTHORIZATION;
    use skyzen_core::Extractor;

    use super::{BearerToken, BearerTokenError};

    #[tokio::test]
    async fn test_bearer_token_extraction() {
        let mut request = http::Request::builder()
            .header(AUTHORIZATION, "Bearer my-secret-token")
            .body(http_kit::Body::empty())
            .unwrap();

        let result = BearerToken::extract(&mut request).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().0, "my-secret-token");
    }

    #[tokio::test]
    async fn test_missing_header() {
        let mut request = http::Request::builder()
            .body(http_kit::Body::empty())
            .unwrap();

        let result = BearerToken::extract(&mut request).await;
        assert!(matches!(result, Err(BearerTokenError::MissingHeader)));
    }

    #[tokio::test]
    async fn test_not_bearer_scheme() {
        let mut request = http::Request::builder()
            .header(AUTHORIZATION, "Basic dXNlcjpwYXNz")
            .body(http_kit::Body::empty())
            .unwrap();

        let result = BearerToken::extract(&mut request).await;
        assert!(matches!(result, Err(BearerTokenError::NotBearer)));
    }

    #[tokio::test]
    async fn test_bearer_without_space() {
        let mut request = http::Request::builder()
            .header(AUTHORIZATION, "Bearertoken")
            .body(http_kit::Body::empty())
            .unwrap();

        let result = BearerToken::extract(&mut request).await;
        assert!(matches!(result, Err(BearerTokenError::NotBearer)));
    }
}
