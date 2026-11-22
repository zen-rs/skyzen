//! API key extraction from a request header (default: `X-API-Key`).

use core::future::{ready, Future};

use http::{header::HeaderName, StatusCode};
use skyzen_core::Secret;

use crate::{extract::Extractor, Request};

/// Default header name used by [`ApiKey`].
pub const DEFAULT_API_KEY_HEADER: HeaderName = HeaderName::from_static("x-api-key");

/// API key extracted from the `X-API-Key` request header.
///
/// The key value is stored as a [`Secret`] so it is never printed by [`Debug`](core::fmt::Debug).
///
/// # Example
///
/// ```rust,ignore
/// use skyzen::extract::ApiKey;
///
/// async fn handler(key: ApiKey) -> &'static str {
///     let _raw = key.expose();
///     "ok"
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ApiKey(pub Secret);

impl ApiKey {
    /// Extract an API key from a custom header name without mutating the request.
    ///
    /// # Errors
    ///
    /// Returns [`ApiKeyError`] if the header is missing, empty, or not valid UTF-8.
    pub fn from_header(request: &Request, header: &HeaderName) -> Result<Self, ApiKeyError> {
        let value = request
            .headers()
            .get(header)
            .ok_or(ApiKeyError::MissingHeader)?
            .to_str()
            .map_err(|_| ApiKeyError::InvalidEncoding)?
            .trim();

        if value.is_empty() {
            return Err(ApiKeyError::EmptyKey);
        }

        Ok(Self(Secret::new(value)))
    }

    /// Expose the raw API key string.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// Borrow the underlying [`Secret`].
    #[must_use]
    pub const fn as_secret(&self) -> &Secret {
        &self.0
    }
}

/// Error returned when extracting an API key fails.
#[skyzen::error(status = StatusCode::UNAUTHORIZED)]
pub enum ApiKeyError {
    /// The API key header is missing from the request.
    #[error("Missing API key header")]
    MissingHeader,
    /// The API key header value is not valid UTF-8.
    #[error("Invalid API key header encoding")]
    InvalidEncoding,
    /// The API key header is empty.
    #[error("Empty API key header")]
    EmptyKey,
}

impl Extractor for ApiKey {
    type Error = ApiKeyError;

    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(Self::from_header(request, &DEFAULT_API_KEY_HEADER))
    }

    #[cfg(feature = "openapi")]
    fn openapi_security() -> Vec<crate::openapi::SecuritySchemeInfo> {
        use utoipa::openapi::security::{ApiKey as UtoipaApiKey, ApiKeyValue, SecurityScheme};

        vec![crate::openapi::SecuritySchemeInfo {
            name: "apiKeyAuth",
            scheme: SecurityScheme::ApiKey(UtoipaApiKey::Header(ApiKeyValue::new("X-API-Key"))),
            optional: false,
        }]
    }
}

#[cfg(test)]
mod tests {
    use http::header::HeaderName;
    use skyzen_core::Extractor;

    use super::{ApiKey, ApiKeyError};

    #[tokio::test]
    async fn extracts_default_x_api_key_and_redacts_debug() {
        let mut request = http::Request::builder()
            .header("x-api-key", "sk_test_12345")
            .body(http_kit::Body::empty())
            .unwrap();

        let key = ApiKey::extract(&mut request).await.unwrap();
        assert_eq!(key.expose(), "sk_test_12345");
        let debug = format!("{key:?}");
        assert!(!debug.contains("sk_test_12345"));
        assert!(debug.contains("Secret(<redacted>)"));
    }

    #[tokio::test]
    async fn extracts_from_custom_header() {
        let request = http::Request::builder()
            .header("x-service-token", "tok_abc")
            .body(http_kit::Body::empty())
            .unwrap();

        let custom = HeaderName::from_static("x-service-token");
        let key = ApiKey::from_header(&request, &custom).unwrap();
        assert_eq!(key.expose(), "tok_abc");
    }

    #[tokio::test]
    async fn rejects_missing_or_empty_key() {
        let mut missing = http::Request::builder()
            .body(http_kit::Body::empty())
            .unwrap();
        assert!(matches!(
            ApiKey::extract(&mut missing).await,
            Err(ApiKeyError::MissingHeader)
        ));

        let mut empty = http::Request::builder()
            .header("x-api-key", "   ")
            .body(http_kit::Body::empty())
            .unwrap();
        assert!(matches!(
            ApiKey::extract(&mut empty).await,
            Err(ApiKeyError::EmptyKey)
        ));
    }
}
