use http_kit::{
    header::{HeaderValue, CONTENT_TYPE},
    Request, Response,
};
use serde::Serialize;
use serde_json::to_vec_pretty;
use skyzen_core::Responder;

/// JSON extractor/responder.It could serialize data as a pretty-printed JSON.
#[derive(Debug, Clone)]
pub struct PrettyJson<T>(pub T);

impl<T: Serialize> Responder for PrettyJson<T> {
    fn respond_to(self, _request: &Request, response: &mut Response) -> http_kit::Result<()> {
        response.replace_body(to_vec_pretty(&self.0)?);
        response.insert_header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(())
    }
}
