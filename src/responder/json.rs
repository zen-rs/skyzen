//! Json responder module.
//! It provides a responder serializing data as pretty-printed JSON.

use http_kit::{
    header::{HeaderValue, CONTENT_TYPE},
    Request, Response,
};
use http_kit::{http_error, StatusCode};
use serde::Serialize;
use serde_json::to_vec_pretty;
use skyzen_core::Responder;

/// A pretty JSON responder,it serialize data as a pretty-printed JSON.
/// # Example
/// ```
/// # use skyzen::responder::PrettyJson;
/// # use serde::Serialize;
/// #[derive(Serialize)]
/// struct User{
///     name:String,
///     age:u8
/// }
/// async fn handler() -> PrettyJson<User>{
///     PrettyJson(User{name:"Lexo".into(),age:17})
/// }
///
/// // Expected result:
/// //{
/// //  "name": "Lexo",
/// //  "age": 17
/// //}
///
/// ```
#[derive(Debug, Clone)]
pub struct PrettyJson<T: Send + Sync + Serialize>(pub T);

http_error!(
    /// An error occurred when serializing the JSON payload.
    pub PrettyJsonError, StatusCode::INTERNAL_SERVER_ERROR, "Failed to serialize JSON payload");

impl<T: Send + Sync + Serialize + 'static> Responder for PrettyJson<T> {
    type Error = PrettyJsonError;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        let payload = to_vec_pretty(&self.0).map_err(|_| PrettyJsonError::new())?;
        *response.body_mut() = http_kit::Body::from_bytes(payload);
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(())
    }
}
