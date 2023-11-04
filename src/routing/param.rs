use async_trait::async_trait;
use http_kit::{Request, StatusCode};
use skyzen_core::Extractor;

/// Extract param defined in route.
#[derive(Debug)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub(crate) fn new(vec: Vec<(String, String)>) -> Self {
        Self(vec)
    }

    pub(crate) const fn empty() -> Self {
        Self(Vec::new())
    }

    /// Get the route parameter by the name.
    pub fn get(&self, name: &str) -> http_kit::Result<&str> {
        let param = self
            .0
            .iter()
            .find_map(|(k, v)| if k == name { Some(v) } else { None })
            .ok_or(
                http_kit::Error::msg(format!("Missing param `{name}`"))
                    .set_status(StatusCode::BAD_REQUEST),
            )?;

        Ok(param.as_str())
    }
}

#[async_trait]
impl Extractor for Params {
    async fn extract(request: &mut Request) -> http_kit::Result<Self> {
        Ok(request.remove_extension().unwrap_or(Self::empty()))
    }
}
