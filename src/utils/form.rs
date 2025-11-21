//! Form utilities module.

use crate::{
    extract::Extractor,
    header::{HeaderValue, CONTENT_TYPE},
    responder::Responder,
    Method, Request, Response, StatusCode,
};

use http_kit::http_error;
use serde::{de::DeserializeOwned, Serialize};
use serde_urlencoded::from_str;

/// Extract form from request body.
#[derive(Debug)]
pub struct Form<T: Send + Sync>(pub T);

#[allow(clippy::declare_interior_mutable_const)]
const APPLICATION_WWW_FORM_URLENCODED: HeaderValue =
    HeaderValue::from_static("application/x-www-form-urlencoded");

http_error!(
    /// Raised when the request content-type is not `application/x-www-form-urlencoded`.
    pub FormEncodeError, StatusCode::SERVICE_UNAVAILABLE, "Failed to parse form data"
);

impl<T: Send + Sync + Serialize + DeserializeOwned + 'static> Responder for Form<T> {
    type Error = FormEncodeError;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        *response.body_mut() =
            http_kit::Body::from_form(&self.0).map_err(|_| FormEncodeError::new())?;
        response
            .headers_mut()
            .insert(CONTENT_TYPE, APPLICATION_WWW_FORM_URLENCODED);
        Ok(())
    }
}

http_error!(
    /// Raised when the request content-type is not `application/x-www-form-urlencoded`.
    pub FormContentTypeError, StatusCode::UNSUPPORTED_MEDIA_TYPE, "Expected content type `application/x-www-form-urlencoded`"
);

impl<T: Send + Sync + DeserializeOwned + 'static> Extractor for Form<T> {
    type Error = FormContentTypeError;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
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
                .map_err(|_| FormContentTypeError::new())?;
            extract(&data)
        }
    }
}

fn extract<T: Send + Sync + DeserializeOwned>(data: &str) -> Result<Form<T>, FormContentTypeError> {
    from_str(data)
        .map(Form)
        .map_err(|_| FormContentTypeError::new())
}

impl_deref!(Form);
