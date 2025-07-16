//! Handle the request and make a response.
//! ```
//! // A simple echo server
//! use bytestr::ByteStr;
//! async fn handler(body:ByteStr) -> http_kit::Result<ByteStr>{
//!    Ok(body)
//! }
//! ```

use core::{future::Future, marker::PhantomData};

use crate::Result;
use http_kit::{Endpoint, Request, Response};
use skyzen_core::{Extractor, Responder};

/// An HTTP handler.
/// This trait is a wrapper trait for `Fn` types. You will rarely use this type directly.
pub trait Handler<T: Extractor>: Send + Sync {
    /// Handle the request and make a response.
    fn call_handler(
        &self,
        request: &mut Request,
    ) -> impl Future<Output = Result<Response>> + Send + Sync;
}

struct IntoEndpoint<H: Handler<T>, T: Extractor> {
    handler: H,
    _marker: PhantomData<T>,
}

/// Transform handler to endpoint.
pub fn into_endpoint<T: Extractor + Send + Sync>(handler: impl Handler<T>) -> impl Endpoint {
    IntoEndpoint::new(handler)
}

impl<H: Handler<T>, T: Extractor> IntoEndpoint<H, T> {
    /// Create an `IntoEndpoint` instance.
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            _marker: PhantomData,
        }
    }
}

macro_rules! impl_handler {
    ($($ty:ident),*) => {
        #[allow(non_snake_case)]

        impl<F, Fut, Res,$($ty:Extractor,)*> Handler<($($ty,)*)> for F
        where
            F: Send+Sync + Fn($($ty,)*) -> Fut,
            Fut: Send + Sync+Future<Output = Res>,
            Res: Responder,
        {

            async fn call_handler(&self, request: &mut Request) -> crate::Result<Response> {
                let ($($ty,)*) = <($($ty,)*) as Extractor>::extract(request).await?;
                let mut response=Response::empty();
                (self)($($ty,)*).await.respond_to(request,&mut response)?;
                Ok(response)
            }
        }
    };
}

tuples!(impl_handler);

impl<H: Handler<T> + Send + Sync, T: Extractor + Send + Sync> Endpoint for IntoEndpoint<H, T> {
    async fn respond(&mut self, request: &mut Request) -> Result<Response> {
        self.handler.call_handler(request).await
    }
}
