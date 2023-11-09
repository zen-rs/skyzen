pub use serde_json::json;
pub use serde_json::Value as JsonValue;

use std::{
    fmt::Display,
    ops::{Deref, DerefMut},
};

use crate::{
    async_trait,
    extract::Extractor,
    header::{HeaderValue, CONTENT_TYPE},
    responder::Responder,
    Request, Response, ResultExt, StatusCode,
};

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{from_str, to_string, to_vec};

/// JSON extractor/responder.
#[derive(Debug, Clone)]
pub struct Json<T = JsonValue>(pub T);

impl<T: Serialize> Responder for Json<T> {
    fn respond_to(self, _request: &Request, response: &mut Response) -> crate::Result<()> {
        response.replace_body(to_vec(&self.0)?);
        response.insert_header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
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
        let body = request.take_body()?;
        if request.get_header(CONTENT_TYPE).ok_or(ContentTypeError)? != "application/json" {
            return Err(ContentTypeError).status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }

        let data = body.into_string().await?;

        Ok(Self(from_str(data.as_str())?))
    }
}

impl<T: Serialize> Display for Json<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = to_string(&self.0).map_err(|_| std::fmt::Error)?;
        f.write_str(&str)?;
        Ok(())
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
