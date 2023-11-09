use http_kit::header::HeaderValue;
pub use serde_json::json;
pub use serde_json::Value as JsonValue;
use std::ops::{Deref, DerefMut};

use crate::{
    async_trait, extract::Extractor, header::CONTENT_TYPE, responder::Responder, Request, Response,
    ResultExt, StatusCode,
};

use serde::{de::DeserializeOwned, Serialize};

/// JSON extractor/responder.
#[derive(Debug, Clone)]
pub struct Json<T = JsonValue>(pub T);

impl<T: Serialize> Responder for Json<T> {
    fn respond_to(self, _request: &Request, response: &mut Response) -> crate::Result<()> {
        response.insert_header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        response.replace_body(serde_json::to_vec(&self.0)?);
        Ok(())
    }
}

impl_error!(
    ContentTypeError,
    "Expected content type `application/json`",
    "This error occurs for a dismatched content type."
);

#[async_trait]
impl<T: DeserializeOwned> Extractor for Json<T> {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        if request.get_header(CONTENT_TYPE).ok_or(ContentTypeError)? != "application/json" {
            return Err(ContentTypeError).status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }

        Ok(Self(request.into_json().await?))
    }
}

impl<T> Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod test {
    use super::Json;
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
                .json(&Lexo {
                    firstname: "Lexo".to_string(),
                    age: 17
                })
                .unwrap()
        );
    }
}
