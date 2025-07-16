use std::future::Future;

use http_kit::{
    utils::{ByteStr, Bytes},
    Body, Method, Request, Result, Uri,
};

/// Extract a object from request,always is the header,body value,etc.
pub trait Extractor: Sized + Send + Sync {
    /// Read the request and parse a value.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self>> + Send + Sync;
}

macro_rules! impl_tuple_extractor {
    ($($ty:ident),*) => {
        #[allow(non_snake_case)]
        #[allow(unused_variables)]
        #[allow(clippy::unused_unit)]
        impl<$($ty:Extractor+Send+Sync,)*> Extractor for ($($ty,)*) {
            async fn extract(request:&mut Request) -> Result<Self>{
                Ok(($($ty::extract(request).await?,)*))
            }
        }
    };
}

tuples!(impl_tuple_extractor);

impl Extractor for Bytes {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.take_body()?.into_bytes().await?)
    }
}

impl Extractor for ByteStr {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.take_body()?.into_string().await?)
    }
}

impl Extractor for Body {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.take_body()?)
    }
}

impl Extractor for Uri {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.uri().clone())
    }
}

impl Extractor for Method {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(request.method().clone())
    }
}

impl<T: Extractor> Extractor for Option<T> {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(T::extract(request).await.ok())
    }
}

impl<T: Extractor> Extractor for Result<T> {
    async fn extract(request: &mut Request) -> Result<Self> {
        Ok(T::extract(request).await)
    }
}
