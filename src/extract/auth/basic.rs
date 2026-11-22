//! HTTP Basic authentication extraction from the `Authorization` header (RFC 7617).

use core::future::{ready, Future};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use http::{header::HeaderValue, StatusCode};
use skyzen_core::{PendingResponseHeaders, Secret};

use crate::{extract::Extractor, header, Request};

/// HTTP Basic credentials extracted from the `Authorization` header.
///
/// The password is stored as a [`Secret`] so it is never printed by [`Debug`](core::fmt::Debug).
///
/// # Example
///
/// ```rust,ignore
/// use skyzen::extract::BasicAuth;
///
/// async fn handler(auth: BasicAuth) -> String {
///     format!("User: {}", auth.username())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct BasicAuth {
    username: String,
    password: Secret,
}

impl BasicAuth {
    /// The decoded username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The decoded password as a redacted [`Secret`].
    #[must_use]
    pub const fn password(&self) -> &Secret {
        &self.password
    }
}

/// Error returned when extracting HTTP Basic credentials fails.
#[skyzen::error(status = StatusCode::UNAUTHORIZED)]
pub enum BasicAuthError {
    /// The `Authorization` header is missing from the request.
    #[error("Missing Authorization header")]
    MissingHeader,
    /// The `Authorization` header value is not valid UTF-8.
    #[error("Invalid Authorization header encoding")]
    InvalidEncoding,
    /// The `Authorization` header does not use the `Basic` scheme.
    #[error("Authorization header must use Basic scheme")]
    NotBasic,
    /// The `Basic` credential is not valid base64 or UTF-8 `username:password`.
    #[error("Malformed Basic credentials")]
    MalformedCredentials,
}

/// Parse HTTP Basic credentials from `request` without mutating it.
///
/// # Errors
///
/// Returns [`BasicAuthError`] if the `Authorization` header is missing, malformed, or not `Basic`.
pub fn parse_basic(request: &Request) -> Result<BasicAuth, BasicAuthError> {
    let value = request
        .headers()
        .get(header::AUTHORIZATION)
        .ok_or(BasicAuthError::MissingHeader)?
        .to_str()
        .map_err(|_| BasicAuthError::InvalidEncoding)?;

    let (scheme, encoded) = value
        .split_once(char::is_whitespace)
        .ok_or(BasicAuthError::NotBasic)?;

    if !scheme.eq_ignore_ascii_case("basic") {
        return Err(BasicAuthError::NotBasic);
    }

    let encoded = encoded.trim();
    if encoded.is_empty() {
        return Err(BasicAuthError::MalformedCredentials);
    }

    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| BasicAuthError::MalformedCredentials)?;
    let decoded = String::from_utf8(decoded).map_err(|_| BasicAuthError::MalformedCredentials)?;
    let (username, password) = decoded
        .split_once(':')
        .ok_or(BasicAuthError::MalformedCredentials)?;

    Ok(BasicAuth {
        username: username.to_owned(),
        password: Secret::new(password),
    })
}

impl Extractor for BasicAuth {
    type Error = BasicAuthError;

    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        let result = parse_basic(request);
        if result.is_err() {
            request
                .extensions_mut()
                .get_or_insert_with(PendingResponseHeaders::default)
                .0
                .entry(header::WWW_AUTHENTICATE)
                .or_insert_with(|| HeaderValue::from_static("Basic"));
        }
        ready(result)
    }

    #[cfg(feature = "openapi")]
    fn openapi_security() -> Vec<crate::openapi::SecuritySchemeInfo> {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

        vec![crate::openapi::SecuritySchemeInfo {
            name: "basicAuth",
            scheme: SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Basic).build()),
            optional: false,
        }]
    }
}

#[cfg(test)]
mod tests {
    use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
    use skyzen_core::{Extractor, PendingResponseHeaders};

    use super::{BasicAuth, BasicAuthError};

    #[tokio::test]
    async fn extracts_basic_credentials_and_redacts_debug() {
        // "alice:s3cr:et" -> "YWxpY2U6czNjcjpldA=="
        let mut request = http::Request::builder()
            .header(AUTHORIZATION, "Basic YWxpY2U6czNjcjpldA==")
            .body(http_kit::Body::empty())
            .unwrap();

        let auth = BasicAuth::extract(&mut request).await.unwrap();
        assert_eq!(auth.username(), "alice");
        assert_eq!(auth.password().expose(), "s3cr:et");
        let debug = format!("{auth:?}");
        assert!(!debug.contains("s3cr:et"));
        assert!(debug.contains("Secret(<redacted>)"));
    }

    #[tokio::test]
    async fn accepts_case_insensitive_scheme() {
        let mut request = http::Request::builder()
            .header(AUTHORIZATION, "basic YWxpY2U6cGFzcw==")
            .body(http_kit::Body::empty())
            .unwrap();

        let auth = BasicAuth::extract(&mut request).await.unwrap();
        assert_eq!(auth.username(), "alice");
        assert_eq!(auth.password().expose(), "pass");
    }

    #[tokio::test]
    async fn missing_header_sets_www_authenticate_basic() {
        let mut request = http::Request::builder()
            .body(http_kit::Body::empty())
            .unwrap();

        let err = BasicAuth::extract(&mut request).await.unwrap_err();
        assert!(matches!(err, BasicAuthError::MissingHeader));
        let pending = request
            .extensions()
            .get::<PendingResponseHeaders>()
            .unwrap();
        assert_eq!(pending.0.get(WWW_AUTHENTICATE).unwrap(), "Basic");
    }

    #[tokio::test]
    async fn rejects_malformed_base64_or_missing_colon() {
        let mut bad_b64 = http::Request::builder()
            .header(AUTHORIZATION, "Basic !!!not-base64!!!")
            .body(http_kit::Body::empty())
            .unwrap();
        assert!(matches!(
            BasicAuth::extract(&mut bad_b64).await,
            Err(BasicAuthError::MalformedCredentials)
        ));

        // "alice" without ':' -> "YWxpY2U="
        let mut no_colon = http::Request::builder()
            .header(AUTHORIZATION, "Basic YWxpY2U=")
            .body(http_kit::Body::empty())
            .unwrap();
        assert!(matches!(
            BasicAuth::extract(&mut no_colon).await,
            Err(BasicAuthError::MalformedCredentials)
        ));
    }
}
