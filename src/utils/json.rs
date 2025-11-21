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
            if content_type != "application/json" {
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
}

#[cfg(test)]
mod test {
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
