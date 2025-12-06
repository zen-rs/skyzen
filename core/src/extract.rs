use core::mem;
use core::{convert::Infallible, future::Future};

#[cfg(feature = "openapi")]
use crate::openapi::{ExtractorSchema, SchemaRef};
use alloc::boxed::Box;
#[cfg(feature = "openapi")]
use alloc::collections::BTreeMap;
use http_kit::error::BoxHttpError;
use http_kit::{
    http_error,
    utils::{ByteStr, Bytes},
    Body, HttpError, Method, Request, StatusCode, Uri,
};

/// Extract a object from request,always is the header,body value,etc.
pub trait Extractor: Sized + Send + Sync + 'static {
    /// Error type returned when extraction fails.
    type Error: HttpError;
    /// Read the request and parse a value.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send;

    /// Describe the extractor's `OpenAPI` schema, if available.
    #[cfg(feature = "openapi")]
    #[must_use]
    fn openapi() -> Option<ExtractorSchema> {
        None
    }

    /// Register dependent schemas into the `OpenAPI` components map.
    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(_defs: &mut BTreeMap<String, SchemaRef>) {}
}

macro_rules! impl_tuple_extractor {
    ($($ty:ident),*) => {
        const _:() = {
            // To prevent these macro-generated errors from overwhelming users.
            #[doc(hidden)]
            pub enum TupleExtractorError<$($ty:Extractor),*> {
                $($ty(<$ty as Extractor>::Error),)*
            }

            impl <$($ty: Extractor),*>core::fmt::Display for TupleExtractorError<$($ty),*> {
                #[allow(unused_variables)]
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    match self {
                        $(TupleExtractorError::$ty(e) => write!(f,"{}",e),)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    }
                }
            }

            impl <$($ty: Extractor),*>core::fmt::Debug for TupleExtractorError<$($ty),*> {
                #[allow(unused_variables)]
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    match self {
                        $(TupleExtractorError::$ty(e) => write!(f,"{:?}",e),)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    }
                }
            }

            impl <$($ty: Extractor),*>core::error::Error for TupleExtractorError<$($ty),*> {}

            impl <$($ty: Extractor),*>http_kit::HttpError for TupleExtractorError<$($ty),*> {
                fn status(&self) -> http_kit::StatusCode {
                    match self {
                        $(TupleExtractorError::$ty(e) => e.status(),)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    }
                }
            }


            #[allow(non_snake_case)]
            #[allow(unused_variables)]
            #[allow(clippy::unused_unit)]
            impl<$($ty:Extractor+Send,)*> Extractor for ($($ty,)*) {
                type Error = TupleExtractorError<$($ty),*>;
            async fn extract(request:&mut Request) -> Result<Self,Self::Error>{
                    Ok(($($ty::extract(request).await.map_err(|error|{
                        TupleExtractorError::$ty(error)
                    })?,)*))
                }
            }

            #[cfg(feature = "openapi")]
            #[allow(dead_code)]
            const fn openapi() -> Option<crate::openapi::ExtractorSchema> {
                None
            }

            #[cfg(feature = "openapi")]
            #[allow(dead_code)]
            const fn register_openapi_schemas(
                _defs: &mut alloc::collections::BTreeMap<String, crate::openapi::SchemaRef>,
            ) {
            }
        };
    };
}

tuples!(impl_tuple_extractor);

http_error!(pub InvalidBody, StatusCode::BAD_REQUEST, "Failed to read request body");

impl Extractor for Bytes {
    type Error = InvalidBody;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let body = mem::replace(request.body_mut(), Body::empty());
        body.into_bytes().await.map_err(|_| InvalidBody::new())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<ExtractorSchema> {
        Some(ExtractorSchema {
            content_type: Some("application/octet-stream"),
            schema: None,
        })
    }
}

impl Extractor for ByteStr {
    type Error = InvalidBody;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        let body = mem::replace(request.body_mut(), Body::empty());
        body.into_string().await.map_err(|_| InvalidBody::new())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<ExtractorSchema> {
        Some(ExtractorSchema {
            content_type: Some("text/plain; charset=utf-8"),
            schema: None,
        })
    }
}

impl Extractor for Body {
    type Error = Infallible;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(mem::replace(request.body_mut(), Self::empty()))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<ExtractorSchema> {
        Some(ExtractorSchema {
            content_type: Some("application/octet-stream"),
            schema: None,
        })
    }
}

impl Extractor for Uri {
    type Error = Infallible;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(request.uri().clone())
    }
}

impl Extractor for Method {
    type Error = Infallible;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(request.method().clone())
    }
}

impl<T: Extractor> Extractor for Option<T> {
    type Error = Infallible;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(T::extract(request).await.ok())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<ExtractorSchema> {
        T::openapi()
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        T::register_openapi_schemas(defs);
    }
}

// Let's erase the error for Result<T,E>, otherwise user have to deal with double error types.
impl<T: Extractor> Extractor for Result<T, BoxHttpError> {
    type Error = Infallible;
    async fn extract(request: &mut Request) -> Result<Self, Self::Error> {
        Ok(T::extract(request)
            .await
            .map_err(|e| Box::new(e) as BoxHttpError))
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<ExtractorSchema> {
        T::openapi()
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        T::register_openapi_schemas(defs);
    }
}
