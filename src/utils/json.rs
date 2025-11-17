use crate::{
    extract::Extractor, header::CONTENT_TYPE, responder::Responder, Request, Response, StatusCode,
};
use http_kit::header::HeaderValue;
use http_kit::ResultExt;
pub use serde_json::json;
pub use serde_json::Value as JsonValue;

use serde::{de::DeserializeOwned, Serialize};

#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// JSON extractor/responder.
#[derive(Debug, Clone)]
pub struct Json<T: Send + Sync = JsonValue>(pub T);

impl<T: Send + Sync + Serialize> Responder for Json<T> {
    fn respond_to(self, _request: &Request, response: &mut Response) -> crate::Result<()> {
        response
            .headers_mut()
            .insert(CONTENT_TYPE, APPLICATION_JSON);
        *response.body_mut() =
            http_kit::Body::from_json(&self.0).status(StatusCode::BAD_REQUEST)?;
        Ok(())
    }
}

/// Error raised when the content-type header is not `application/json`.
#[derive(Debug)]
#[allow(dead_code)]
#[skyzen::error(message = "Expected content type `application/json`")]
pub struct JsonContentTypeError;

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Json<T> {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        if let Some(content_type) = request.headers().get(CONTENT_TYPE) {
            if content_type != "application/json" {
                return Err(JsonContentTypeError).status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
            }
        } else {
            return Err(JsonContentTypeError).status(StatusCode::BAD_REQUEST);
        }

        let value = request
            .body_mut()
            .into_json()
            .await
            .map_err(|error| http_kit::Error::new(error, StatusCode::BAD_REQUEST))?;
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
