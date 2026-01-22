//! Bearer token extraction from Authorization header.

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

impl Extractor for BearerToken {
    type Error = BearerTokenError;

    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let header_value = request
            .headers()
            .get(header::AUTHORIZATION)
            .ok_or(BearerTokenError::MissingHeader)?;

        let value = header_value
            .to_str()
            .map_err(|_| BearerTokenError::InvalidEncoding)?;

        let token = value
            .strip_prefix("Bearer ")
            .ok_or(BearerTokenError::NotBearer)?;

        Ok(Self(token.to_owned()))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
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
