use std::ops::{Deref, DerefMut};

use crate::{
    async_trait,
    extract::Extractor,
    header::{HeaderValue, CONTENT_TYPE},
    responder::Responder,
    Method, Request, Response, ResultExt, StatusCode,
};

use serde::{de::DeserializeOwned, Serialize};
use serde_urlencoded::{from_str, to_string};

/// Extract form from request body.
#[derive(Debug)]
pub struct Form<T>(pub T);

const APPLICATION_WWW_FORM_URLENCODED: HeaderValue =
    HeaderValue::from_static("application/x-www-form-urlencoded");

impl<T: Serialize> Responder for Form<T> {
    fn respond_to(self, _request: &Request, response: &mut Response) -> crate::Result<()> {
        response.replace_body(to_string(&self.0)?);
        response.insert_header(CONTENT_TYPE, APPLICATION_WWW_FORM_URLENCODED);
        Ok(())
    }
}

impl_error!(
    ContentTypeError,
    "Expected content type `application/x-www-form-urlencoded`",
    "This error occurs for a dismatched content type."
);

#[async_trait]
impl<T: DeserializeOwned> Extractor for Form<T> {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        if request.method() == Method::GET {
            let data = request.uri().query().unwrap_or_default();
            Ok(Self(from_str(data).status(StatusCode::BAD_REQUEST)?))
        } else {
            if request.get_header(CONTENT_TYPE).ok_or(ContentTypeError)?
                != APPLICATION_WWW_FORM_URLENCODED
            {
                return Err(ContentTypeError).status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
            }
            let body = request.take_body()?;
            let data = body.into_string().await?;
            Ok(Self(
                serde_urlencoded::from_str(data.as_str()).status(StatusCode::BAD_REQUEST)?,
            ))
        }
    }
}

impl<T> Deref for Form<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for Form<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
