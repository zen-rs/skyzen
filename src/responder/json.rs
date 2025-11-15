use http_kit::{
    header::{HeaderValue, CONTENT_TYPE},
    Request, Response,
};
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

impl<T: Send + Sync + Serialize> Responder for PrettyJson<T> {
    fn respond_to(self, _request: &Request, response: &mut Response) -> http_kit::Result<()> {
        *response.body_mut() = http_kit::Body::from_bytes(to_vec_pretty(&self.0)?);
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(())
    }
}
