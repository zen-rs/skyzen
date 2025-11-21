use std::convert::Infallible;
use std::fmt;

use http_kit::{HttpError, Request, StatusCode};
use skyzen_core::Extractor;

/// Extract param defined in route.
#[derive(Debug, Clone)]
pub struct Params(Vec<(String, String)>);

/// Error returned when attempting to read a missing route parameter.
#[derive(Debug, Clone)]
pub struct MissingParam {
    name: String,
}

impl MissingParam {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl fmt::Display for MissingParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Missing param `{}`", self.name)
    }
}

impl std::error::Error for MissingParam {}

impl HttpError for MissingParam {
    fn status(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl Params {
    pub(crate) const fn new(vec: Vec<(String, String)>) -> Self {
        Self(vec)
    }

    pub(crate) const fn empty() -> Self {
        Self(Vec::new())
    }

    /// Get the route parameter by the name.
    ///
    /// # Errors
    ///
    /// Returns an error if the requested parameter is not present.
    pub fn get(&self, name: &str) -> Result<&str, MissingParam> {
        self.0
            .iter()
            .find_map(|(k, v)| if k == name { Some(v.as_str()) } else { None })
            .ok_or_else(|| MissingParam::new(name))
    }
}

impl Extractor for Params {
    type Error = Infallible;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(request
            .extensions_mut()
            .remove::<Self>()
            .unwrap_or(Self::empty()))
    }
}
