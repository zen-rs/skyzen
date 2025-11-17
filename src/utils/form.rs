use crate::{
    extract::Extractor,
    header::{HeaderValue, CONTENT_TYPE},
    responder::Responder,
    Method, Request, Response, StatusCode,
};

use http_kit::{Error, ResultExt};
use serde::{de::DeserializeOwned, Serialize};
use serde_urlencoded::from_str;

/// Extract form from request body.
#[derive(Debug)]
pub struct Form<T: Send + Sync>(pub T);

#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_WWW_FORM_URLENCODED: HeaderValue =
    HeaderValue::from_static("application/x-www-form-urlencoded");

impl<T: Send + Sync + Serialize + DeserializeOwned> Responder for Form<T> {
    fn respond_to(self, _request: &Request, response: &mut Response) -> crate::Result<()> {
        *response.body_mut() =
            http_kit::Body::from_form(&self.0).status(StatusCode::BAD_REQUEST)?;
        response
            .headers_mut()
            .insert(CONTENT_TYPE, APPLICATION_WWW_FORM_URLENCODED);
        Ok(())
    }
}

/// Raised when the request content-type is not `application/x-www-form-urlencoded`.
#[derive(Debug)]
#[allow(dead_code)]
#[skyzen::error(
    status = StatusCode::UNSUPPORTED_MEDIA_TYPE,
    message = "Expected content type `application/x-www-form-urlencoded`"
)]
pub struct FormContentTypeError;

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Form<T> {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        /*if request
            .get_header(CONTENT_TYPE)
            .ok_or(FormContentTypeError)?
            != APPLICATION_WWW_FORM_URLENCODED
        {
            return Err(FormContentTypeError).status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }*/
        // TODO!

        if request.method() == Method::GET {
            let data = request.uri().query().unwrap_or_default();
            extract(data)
        } else {
            let body = core::mem::replace(request.body_mut(), http_kit::Body::empty());
            let data = body
                .into_string()
                .await
                .map_err(|error| http_kit::Error::new(error, StatusCode::BAD_REQUEST))?;
            extract(&data)
        }
    }
}

fn extract<T: Send + Sync + DeserializeOwned>(data: &str) -> Result<Form<T>, Error> {
    Ok(Form(from_str(data).map_err(|_| {
        http_kit::Error::msg("Form content type error")
            .set_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
    })?))
}

impl_deref!(Form);
