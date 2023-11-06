use async_trait::async_trait;
use bytes::Bytes;
use bytestr::ByteStr;
use http::{Method, Uri};
use http_kit::{Body, Request, Result};

/// Extract a object from request,always is the header,body value,etc.
#[async_trait]
pub trait Extractor: Sized {
    /// Read the request and parse a value.
    async fn extract(request: &mut Request) -> Result<Self>;
}

macro_rules! impl_tuple_extractor {
    ($($ty:ident),*) => {
        #[allow(non_snake_case)]
        #[allow(unused_variables)]
        #[allow(clippy::unused_unit)]
        #[async_trait]
        impl<$($ty:Extractor+Send,)*> Extractor for ($($ty,)*) {
            async fn extract(request:&mut Request) -> Result<Self>{
                Ok(($($ty::extract(request).await?,)*))
            }
        }
    };
}

tuples!(impl_tuple_extractor);

#[async_trait]
impl Extractor for Bytes {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.take_body()?.into_bytes().await?)
    }
}

#[async_trait]
impl Extractor for ByteStr {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.take_body()?.into_string().await?)
    }
}

#[async_trait]
impl Extractor for Body {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.take_body()?)
    }
}

#[async_trait]
impl Extractor for Uri {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.uri().clone())
    }
}

#[async_trait]
impl Extractor for Method {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.method().clone())
    }
}

#[async_trait]
impl<T: Extractor> Extractor for Option<T> {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(T::extract(request).await.ok())
    }
}

#[async_trait]
impl<T: Extractor> Extractor for Result<T> {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(T::extract(request).await)
    }
}
