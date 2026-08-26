use core::any::{type_name, TypeId};
use core::mem;
use core::{convert::Infallible, future::Future};

#[cfg(feature = "openapi")]
use crate::openapi::{ExtractorSchema, ParameterLocation, SchemaRef};
use alloc::boxed::Box;
#[cfg(feature = "openapi")]
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use http_kit::error::BoxHttpError;
use http_kit::{
    http_error,
    utils::{ByteStr, Bytes},
    Body, HttpError, Method, Request, StatusCode, Uri,
};

/// A value an extractor needs some middleware to have put into the request.
///
/// Extractors that read a value back out of the request extensions declare it here so the route
/// tree can be checked at build time instead of failing with a 500 on the first request that
/// reaches the endpoint. Report a requirement only when route-attached middleware is the *only*
/// way the value can arrive; a value that a runtime, a test harness or a generated entrypoint may
/// also inject cannot be validated this way and must not be declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    type_id: TypeId,
    description: &'static str,
    hint: &'static str,
}

impl Requirement {
    /// Declare that `T` must be present in the request extensions, suggesting `hint` as the fix.
    #[must_use]
    pub fn of<T: 'static>(hint: &'static str) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            description: type_name::<T>(),
            hint,
        }
    }

    /// The type that must be present.
    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// A human-readable name for the missing type.
    #[must_use]
    pub const fn description(&self) -> &'static str {
        self.description
    }

    /// The call that would satisfy this requirement, quoted for an error message.
    #[must_use]
    pub const fn hint(&self) -> &'static str {
        self.hint
    }
}

/// Extracts a typed value from an HTTP request, such as a header, the body, or
/// other request metadata.
pub trait Extractor: Sized + Send + Sync + 'static {
    /// Error type returned when extraction fails.
    type Error: HttpError;
    /// Read the request and parse a value.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send;

    /// Values this extractor needs route middleware to provide.
    ///
    /// Defaults to nothing. See [`Requirement`] for when declaring one is sound.
    #[must_use]
    fn requirements() -> Vec<Requirement> {
        Vec::new()
    }

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
            impl<$($ty:Extractor,)*> Extractor for ($($ty,)*) {
                type Error = TupleExtractorError<$($ty),*>;
                async fn extract(request:&mut Request) -> Result<Self,Self::Error>{
                    Ok(($($ty::extract(request).await.map_err(|error|{
                        TupleExtractorError::$ty(error)
                    })?,)*))
                }

                fn requirements() -> alloc::vec::Vec<crate::extract::Requirement> {
                    #[allow(unused_mut)]
                    let mut requirements = alloc::vec::Vec::new();
                    $(requirements.extend(<$ty as Extractor>::requirements());)*
                    requirements
                }

                // A tuple combines several extractors, but `openapi()` can only describe one, so it
                // reports none — document tuple-sourced parameters via individual handler arguments
                // instead. Component schemas are still registered via `register_openapi_schemas`.
                #[cfg(feature = "openapi")]
                fn openapi() -> Option<crate::openapi::ExtractorSchema> {
                    None
                }

                #[cfg(feature = "openapi")]
                fn register_openapi_schemas(
                    defs: &mut alloc::collections::BTreeMap<String, crate::openapi::SchemaRef>,
                ) {
                    $(<$ty as Extractor>::register_openapi_schemas(defs);)*
                }
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
            location: ParameterLocation::Body,
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
            location: ParameterLocation::Body,
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
            location: ParameterLocation::Body,
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

// `requirements()` is deliberately *not* forwarded here or on `Result<T, BoxHttpError>`: both
// wrappers exist so the handler can cope with `T` being unavailable, so a missing provision is a
// case the handler asked to see rather than a wiring mistake to reject at build time.
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
