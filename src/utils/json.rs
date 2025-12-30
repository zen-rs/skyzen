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

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Json<T> {
    type Error = JsonContentTypeError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
            if !is_json_content_type(content_type) {
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

fn is_json_content_type(value: &HeaderValue) -> bool {
    value
        .to_str()
        .ok()
        .and_then(|raw| raw.split(';').next())
        .map(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        .unwrap_or(false)
}

#[cfg(test)]
mod test {
    use super::Json;
    use crate::{Body, Method, StatusCode};
    use http_kit::{header::CONTENT_TYPE, HttpError, Request};
    use serde::Deserialize;
    use skyzen_core::Extractor;

    #[derive(Debug, Deserialize)]
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

    /* use super::Json;
    use http_kit::Request;
    use serde::{Deserialize, Serialize};
    #[derive(Debug, Serialize, Deserialize)]
    struct Lexo {
        firstname: String,
        age: u8,
    }

    #[test]
    fn serialize() {
        async fn handler() -> Json<Lexo> {
            Json(Lexo {
                firstname: "Lexo".to_string(),
                age: 17,
            })
        }

        test_handler!(handler, r#"{"firstname":"Lexo","age":17}"#.to_string());
    }

    #[test]
    fn deserialize() {
        async fn handler(Json(lexo): Json<Lexo>) -> String {
            let firstname = lexo.firstname;
            format!("Hello,{firstname}!")
        }

        test_handler!(
            handler,
            "Hello,Lexo!",
            request = Request::post("http://localhost:8080/")
                .json(Lexo {
                    firstname: "Lexo".to_string(),
                    age: 17
                })
                .unwrap()
        );
    }*/
}
