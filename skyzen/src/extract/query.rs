use crate::{async_trait, extract::Extractor, Request, ResultExt, StatusCode};

use serde::de::DeserializeOwned;
use serde_urlencoded::from_str;

/// Parse query from Uri.
#[derive(Debug, Clone)]
pub struct Query<T>(pub T);

impl_deref!(Query);

#[async_trait]
impl<T: DeserializeOwned> Extractor for Query<T> {
    async fn extract(request: &mut Request) -> crate::Result<Self> {
        let data = request.uri().query().unwrap_or_default();
        Ok(Self(from_str(data).status(StatusCode::BAD_REQUEST)?))
    }
}
