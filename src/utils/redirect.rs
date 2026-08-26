//! Redirect responses.

use crate::{
    header::{HeaderValue, LOCATION},
    Request, Response, StatusCode,
};
use skyzen_core::Responder;

/// Answers with a redirect to another location.
///
/// Each constructor names the status it sends, because the choice is a behavioural one:
///
/// | Constructor | Status | The client should |
/// |---|---|---|
/// | [`Redirect::to`] | `302 Found` | follow, keeping the original method in practice |
/// | [`Redirect::see_other`] | `303 See Other` | follow with `GET`, whatever the original method was |
/// | [`Redirect::temporary`] | `307 Temporary Redirect` | follow with the *same* method and body |
/// | [`Redirect::permanent`] | `308 Permanent Redirect` | follow with the same method, and remember |
///
/// After a successful `POST`, [`see_other`](Self::see_other) is the one you want: it is what stops
/// a reload from resubmitting the form. Reach for [`temporary`](Self::temporary) or
/// [`permanent`](Self::permanent) when the method must survive the hop, and for
/// [`to`](Self::to) when you want the historically permissive `302` that browsers turn into a
/// `GET` anyway.
///
/// ```rust
/// use skyzen::utils::Redirect;
///
/// async fn create() -> Redirect {
///     Redirect::see_other("/articles/1")
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Redirect {
    status: StatusCode,
    location: String,
}

impl Redirect {
    /// Redirect with `302 Found`.
    #[must_use]
    pub fn to(location: impl Into<String>) -> Self {
        Self::with_status(StatusCode::FOUND, location)
    }

    /// Redirect with `303 See Other`, telling the client to follow with `GET`.
    #[must_use]
    pub fn see_other(location: impl Into<String>) -> Self {
        Self::with_status(StatusCode::SEE_OTHER, location)
    }

    /// Redirect with `307 Temporary Redirect`, preserving the method and body.
    #[must_use]
    pub fn temporary(location: impl Into<String>) -> Self {
        Self::with_status(StatusCode::TEMPORARY_REDIRECT, location)
    }

    /// Redirect with `308 Permanent Redirect`, preserving the method and body.
    #[must_use]
    pub fn permanent(location: impl Into<String>) -> Self {
        Self::with_status(StatusCode::PERMANENT_REDIRECT, location)
    }

    /// Redirect with an explicitly chosen status.
    #[must_use]
    pub fn with_status(status: StatusCode, location: impl Into<String>) -> Self {
        Self {
            status,
            location: location.into(),
        }
    }

    /// The status this redirect sends.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// The location this redirect points at.
    #[must_use]
    pub fn location(&self) -> &str {
        &self.location
    }
}

/// Raised when a redirect target cannot be sent as a `Location` header.
///
/// A location is server-supplied, so a target carrying a newline or a non-ASCII byte is a bug in
/// the application rather than something the caller did: it reports `500` and the offending value
/// stays in the logs.
#[skyzen::error(
    message = "Redirect location `{0}` cannot be sent as a header value",
    status = StatusCode::INTERNAL_SERVER_ERROR
)]
pub struct InvalidRedirectLocation(String);

impl Responder for Redirect {
    type Error = InvalidRedirectLocation;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        let value = HeaderValue::from_str(&self.location)
            .map_err(|_| InvalidRedirectLocation(self.location.clone()))?;
        *response.status_mut() = self.status;
        response.headers_mut().insert(LOCATION, value);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<crate::openapi::ResponseSchema>> {
        // The status is chosen per value, so only the shape of the response is documented here.
        Some(vec![crate::openapi::ResponseSchema {
            status: None,
            description: Some("Redirect to another location"),
            schema: None,
            content_type: None,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::Redirect;
    use crate::{header::LOCATION, Body, Request, Response, StatusCode};
    use http_kit::HttpError;
    use skyzen_core::Responder;

    fn respond(redirect: Redirect) -> Response {
        let mut response = Response::new(Body::empty());
        redirect
            .respond_to(&Request::new(Body::empty()), &mut response)
            .expect("a plain path is a valid header value");
        response
    }

    #[test]
    fn each_constructor_sends_its_documented_status() {
        for (redirect, expected) in [
            (Redirect::to("/a"), StatusCode::FOUND),
            (Redirect::see_other("/a"), StatusCode::SEE_OTHER),
            (Redirect::temporary("/a"), StatusCode::TEMPORARY_REDIRECT),
            (Redirect::permanent("/a"), StatusCode::PERMANENT_REDIRECT),
        ] {
            let response = respond(redirect);
            assert_eq!(response.status(), expected);
            assert_eq!(response.headers().get(LOCATION).unwrap(), "/a");
        }
    }

    #[test]
    fn a_location_that_cannot_be_a_header_value_is_a_server_error() {
        let mut response = Response::new(Body::empty());
        let error = Redirect::to("/bad\nheader")
            .respond_to(&Request::new(Body::empty()), &mut response)
            .unwrap_err();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
