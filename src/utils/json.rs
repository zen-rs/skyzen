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

#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// JSON extractor/responder.
#[derive(Debug, Clone)]
pub struct Json<T: Send + Sync + 'static = JsonValue>(pub T);

http_error!(
    /// An error occurred when encoding the JSON response.
    pub JsonEncodingError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to encode JSON response");

impl<T: Send + Sync + Serialize + 'static> Responder for Json<T> {
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
            schema: None,
            content_type: Some("application/json"),
        }])
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        _defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
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
    #[error("Failed to parse JSON payload", status = StatusCode::BAD_REQUEST)]
    InvalidPayload,
}

fn is_application_json(content_type: &HeaderValue) -> bool {
    let Ok(value) = content_type.to_str() else {
        return false;
    };

    let Some(media_type) = value.split(';').next().map(str::trim) else {
        return false;
    };

    media_type.eq_ignore_ascii_case("application/json")
}

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Json<T> {
    type Error = JsonContentTypeError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
            if !is_application_json(content_type) {
                return Err(JsonContentTypeError::Unsupported);
            }
        } else {
            return Err(JsonContentTypeError::Missing);
        }

        let value = request
            .body_mut()
            .into_json()
            .await
            .map_err(|_| JsonContentTypeError::InvalidPayload)?;
        Ok(Self(value))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<crate::openapi::ExtractorSchema> {
        Some(crate::openapi::ExtractorSchema {
            content_type: Some("application/json"),
            schema: None,
        })
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(
        _defs: &mut std::collections::BTreeMap<String, crate::openapi::SchemaRef>,
    ) {
    }
}

#[cfg(test)]
mod test {
    use http_kit::header::HeaderValue;

    use super::is_application_json;

    #[test]
    fn accepts_content_type_without_parameters() {
        assert!(is_application_json(&HeaderValue::from_static(
            "application/json"
        )));
    }

    #[test]
    fn accepts_content_type_with_parameters() {
        assert!(is_application_json(&HeaderValue::from_static(
            "application/json; charset=utf-8"
        )));
    }

    #[test]
    fn rejects_non_json_content_type() {
        assert!(!is_application_json(&HeaderValue::from_static(
            "text/plain"
        )));
    }
}
