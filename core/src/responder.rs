use alloc::borrow::Cow;
use alloc::boxed::Box;
#[cfg(feature = "openapi")]
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::pin::Pin;
use http_kit::error::BoxHttpError;
use http_kit::header::{HeaderMap, HeaderName, HeaderValue};
use http_kit::HttpError;
use http_kit::{
    utils::{AsyncBufRead, ByteStr, Bytes},
    Body, Request, Response,
};

#[cfg(feature = "openapi")]
use crate::openapi::{ResponseSchema, SchemaRef};
use crate::Error;

/// Transform a object into a part of HTTP response,always is response body,header,etc.
pub trait Responder: Sized + Send + Sync + 'static {
    /// Error type returned when responding fails.
    type Error: HttpError;
    /// Modify the response,sometime also read the request (but the body may have already been consumed).
    ///
    /// # Errors
    ///
    /// Returns an error if the response fails.
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error>;

    /// Describe the responder's `OpenAPI` schemas, if available.
    #[cfg(feature = "openapi")]
    #[must_use]
    fn openapi() -> Option<Vec<ResponseSchema>> {
        None
    }

    /// Register dependent schemas into the `OpenAPI` components map.
    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(_defs: &mut BTreeMap<String, SchemaRef>) {}
}

macro_rules! impl_tuple_responder {
        ($($ty:ident),*) => {
            const _:() = {
                    // To prevent these macro-generated errors from overwhelming users.
            #[doc(hidden)]
            pub enum TupleResponderError<$($ty:Responder),*> {
                $($ty(<$ty as Responder>::Error),)*
            }

            impl <$($ty: Responder),*>core::fmt::Display for TupleResponderError<$($ty),*> {
                #[allow(unused_variables)]
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    match self {
                        $(TupleResponderError::$ty(e) => write!(f,"{}",e),)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    }
                }
            }

            impl<$($ty: Responder),*>core::fmt::Debug for TupleResponderError<$($ty),*> {
                #[allow(unused_variables)]
                fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                    match self {
                        $(TupleResponderError::$ty(e) => write!(f,"{:?}",e),)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    }
                }
            }

            impl <$($ty: Responder),*>core::error::Error for TupleResponderError<$($ty),*> {}

            impl<$($ty: Responder),*>http_kit::HttpError for TupleResponderError<$($ty),*> {
                fn status(&self) -> http_kit::StatusCode { {
                    match self {
                        $(TupleResponderError::$ty(e) => e.status(),)*
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    }
                } }
            }


                #[allow(non_snake_case)]
                #[allow(unused_variables)]
                impl <$($ty:Responder,)*>Responder for ($($ty,)*){
                    type Error=TupleResponderError<$($ty),*>;
                    fn respond_to(self, request: &Request,response:&mut Response) -> Result<(),Self::Error>{
                        let ($($ty,)*)=self;
                        $($ty.respond_to(request,response).map_err(|e| TupleResponderError::$ty(e))?;)*
                        Ok(())
                    }

                    #[cfg(feature = "openapi")]
                    fn openapi() -> Option<Vec<ResponseSchema>> {
                        #[allow(unused_mut)]
                        let mut schemas = Vec::new();
                        $(
                            if let Some(mut inner) = <$ty as Responder>::openapi() {
                                schemas.append(&mut inner);
                            }
                        )*
                        if schemas.is_empty() { None } else { Some(schemas) }
                    }

                    #[cfg(feature = "openapi")]
                    fn register_openapi_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
                        $(
                            <$ty as Responder>::register_openapi_schemas(defs);
                        )*
                    }
                }
            };
        };
}

tuples!(impl_tuple_responder);

impl_base_responder![Bytes, Vec<u8>, Body, &'static [u8], Cow<'static, [u8]>];

impl Responder for Pin<Box<dyn AsyncBufRead + Send + Sync + 'static>> {
    type Error = core::convert::Infallible;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        *response.body_mut() = Body::from_reader(self, None);
        Ok(())
    }
}

impl_base_utf8_responder![ByteStr, String, &'static str, Cow<'static, str>];

impl Responder for Response {
    type Error = core::convert::Infallible;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        *response = self;
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

impl<T: Responder, E: HttpError> Responder for core::result::Result<T, E> {
    type Error = BoxHttpError;
    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        match self {
            Ok(responder) => responder
                .respond_to(request, response)
                .map_err(|e| Box::new(e) as BoxHttpError),
            Err(e) => Err(Box::new(e) as BoxHttpError),
        }
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<ResponseSchema>> {
        let mut schemas = Vec::new();
        if let Some(mut inner) = T::openapi() {
            schemas.append(&mut inner);
        }
        schemas.push(ResponseSchema {
            status: Some(http_kit::StatusCode::SERVICE_UNAVAILABLE),
            description: None,
            schema: None,
            content_type: None,
        });
        if schemas.is_empty() {
            None
        } else {
            Some(schemas)
        }
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        T::register_openapi_schemas(defs);
    }
}

impl<T: Responder> Responder for core::result::Result<T, Error> {
    type Error = BoxHttpError;
    fn respond_to(self, request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        match self {
            Ok(responder) => responder
                .respond_to(request, response)
                .map_err(|e| Box::new(e) as BoxHttpError),
            Err(e) => Err(e.into_boxed_http_error()),
        }
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<ResponseSchema>> {
        let mut schemas = Vec::new();
        if let Some(mut inner) = T::openapi() {
            schemas.append(&mut inner);
        }
        if schemas.is_empty() {
            None
        } else {
            Some(schemas)
        }
    }

    #[cfg(feature = "openapi")]
    fn register_openapi_schemas(defs: &mut BTreeMap<String, SchemaRef>) {
        T::register_openapi_schemas(defs);
    }
}

impl Responder for HeaderMap {
    type Error = Infallible;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        response.headers_mut().extend(self);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}

impl Responder for (HeaderName, HeaderValue) {
    type Error = Infallible;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        let (key, value) = self;
        response.headers_mut().append(key, value);
        Ok(())
    }

    #[cfg(feature = "openapi")]
    fn openapi() -> Option<Vec<ResponseSchema>> {
        Some(vec![ResponseSchema {
            status: None,
            description: None,
            schema: None,
            content_type: None,
        }])
    }
}
