//! JSON utilities module.
//! It provides JSON extractor and responder.

use crate::{
    extract::Extractor, header::CONTENT_TYPE, responder::Responder, Request, Response, StatusCode,
};
use http_kit::header::HeaderValue;
use http_kit::http_error;
pub use serde_json::json;
pub use serde_json::Value as JsonValue;

use serde::{de::DeserializeOwned, Serialize};
use skyzen_core::{take_body_bytes, BodyExtractorError};

#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// JSON extractor/responder.
#[derive(Debug, Clone)]
pub struct Json<T: Send + Sync + 'static = JsonValue>(pub T);

/// What a [`Json`] extraction can fail with: the body was unavailable, or it was not JSON.
pub type JsonRejection = BodyExtractorError<JsonContentTypeError>;

http_error!(
    /// An error occurred when encoding the JSON response.
    pub JsonEncodingError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to encode JSON response");

impl<T: Send + Sync + Serialize + crate::ToSchema + 'static> Responder for Json<T> {
    type Error = JsonEncodingError;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, APPLICATION_JSON);
        *response.body_mut() =
            http_kit::Body::from_json(&self.0).map_err(|_| JsonEncodingError::new())?;
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<crate::openapi::ResponseSchema>> {
        Some(vec![crate::openapi::ResponseSchema {
            status: None,
            description: None,
            schema: crate::openapi::schema_of::<T>(),
            content_type: Some("application/json"),
        }])
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::register_schema_for::<T>(defs);
    }
}

/// Error raised when the content-type header is not `application/json`.
#[skyzen::error]
pub enum JsonContentTypeError {
    /// The content type header is missing.
    #[error("Expected content type `application/json`", status = StatusCode::BAD_REQUEST)]
    Missing,
    /// The content type does not match `application/json`.
    #[error(
        "Expected content type `application/json`",
        status = StatusCode::UNSUPPORTED_MEDIA_TYPE
    )]
    Unsupported,
    /// The payload could not be parsed as JSON.
    ///
    /// Carries the deserializer's own message so the 4xx response tells the caller which field
    /// was wrong, rather than only saying that something was.
    #[error("Failed to parse JSON payload: {0}", status = StatusCode::BAD_REQUEST)]
    InvalidPayload(String),
}

impl<T: Send + Sync + DeserializeOwned + crate::ToSchema + 'static> Extractor for Json<T> {
    type Error = JsonRejection;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
            if !is_json_content_type(content_type) {
                return Err(JsonRejection::Parse(JsonContentTypeError::Unsupported));
            }
        } else {
            return Err(JsonRejection::Parse(JsonContentTypeError::Missing));
        }

        let bytes = take_body_bytes::<Self>(request).await?;
        let value = serde_json::from_slice(&bytes).map_err(|error| {
            tracing::debug!(%error, "failed to deserialize JSON request body");
            JsonRejection::Parse(JsonContentTypeError::InvalidPayload(error.to_string()))
        })?;
        Ok(Self(value))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            location: crate::openapi::ParameterLocation::Body,
            content_type: Some("application/json"),
            schema: crate::openapi::schema_of::<T>(),
        })
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
        crate::openapi::register_schema_for::<T>(defs);
    }
}

/// Accept `application/json` as well as any `application/*+json` media type
/// (e.g. `application/problem+json`).
fn is_json_content_type(value: &HeaderValue) -> bool {
    value
        .to_str()
        .ok()
        .and_then(|raw| raw.split(';').next())
        .is_some_and(|mime| {
            const PREFIX: &[u8] = b"application/";
            const SUFFIX: &[u8] = b"+json";
            let mime = mime.trim();
            if mime.eq_ignore_ascii_case("application/json") {
                return true;
            }
            let bytes = mime.as_bytes();
            bytes.len() > PREFIX.len() + SUFFIX.len()
                && bytes[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
                && bytes[bytes.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
        })
}

#[cfg(test)]
mod test {
    use super::Json;
    use crate::{Body, Method, StatusCode};
    use http_kit::{header::CONTENT_TYPE, HttpError, Request};
    use serde::Deserialize;
    use skyzen_core::Extractor;

    #[derive(Debug, Deserialize, crate::ToSchema)]
    struct Payload {
        ok: bool,
    }

    fn request_with_body(body: &'static [u8]) -> Request {
        let mut request = Request::new(Body::from_bytes(body.to_vec()));
        *request.method_mut() = Method::POST;
        *request.uri_mut() = "http://localhost/".parse().expect("invalid uri");
        request
    }

    #[tokio::test]
    async fn accepts_charset_param() {
        let mut request = request_with_body(br#"{"ok":true}"#);
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/json; charset=utf-8"),
        );

        let Json(payload) = Json::<Payload>::extract(&mut request)
            .await
            .expect("json should parse");
        assert!(payload.ok);
    }

    #[tokio::test]
    async fn accepts_json_suffix_media_types() {
        let mut request = request_with_body(br#"{"ok":true}"#);
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/problem+json"),
        );

        let Json(payload) = Json::<Payload>::extract(&mut request)
            .await
            .expect("json should parse");
        assert!(payload.ok);
    }

    #[tokio::test]
    async fn rejects_invalid_json_payload_with_bad_request() {
        let mut request = request_with_body(b"{not json");
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/json"),
        );

        let error = Json::<Payload>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let message = error.to_string();
        assert!(
            message.starts_with("Failed to parse JSON payload: "),
            "rejection should keep its prefix, got {message}"
        );
        assert!(
            message.contains("line 1"),
            "rejection should carry the deserializer detail, got {message}"
        );
    }

    #[tokio::test]
    async fn rejects_non_json_content_type() {
        let mut request = request_with_body(br#"{"ok":true}"#);
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("text/plain"),
        );

        let error = Json::<Payload>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn rejects_missing_content_type() {
        let mut request = request_with_body(br#"{"ok":true}"#);
        let error = Json::<Payload>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_oversized_json_body_is_rejected_with_413() {
        let mut request = request_with_body(br#"{"ok":true}"#);
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/json"),
        );
        request
            .extensions_mut()
            .insert(skyzen_core::RequestBodyLimit::new(4));

        let error = Json::<Payload>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn a_second_body_read_names_both_extractors() {
        let mut request = request_with_body(br#"{"ok":true}"#);
        request.headers_mut().insert(
            CONTENT_TYPE,
            http_kit::header::HeaderValue::from_static("application/json"),
        );

        Json::<Payload>::extract(&mut request)
            .await
            .expect("first read succeeds");
        let error = Json::<Payload>::extract(&mut request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            error.to_string().contains("already consumed by"),
            "rejection should explain the double read, got {error}"
        );
    }
}
