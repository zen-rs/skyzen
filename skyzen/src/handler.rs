//! Handle the request and make a response.
//! ```
//! // A simple echo server
//! async fn handler(body:ByteStr) -> http_kit::Result<ByteStr>{
//!    Ok(body)
//! }
//! ```

use std::{future::Future, marker::PhantomData, pin::Pin};

use crate::Result;
use async_trait::async_trait;
use http_kit::{Endpoint, Request, Response};
use skyzen_core::{Extractor, Responder};

/// An HTTP handler.
/// This trait is a wrapper trait for `Fn` types. You will rarely use this type directly.
#[async_trait]
pub trait Handler<T: Extractor>: Send + Sync {
    /// Handle the request and make a response.
    async fn call_handler(&self, request: &mut Request) -> Result<Response>;
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
        #[async_trait]
        impl<F, Fut, Res,$($ty:Send+Extractor,)*> Handler<($($ty,)*)> for F
        where
            F: Send+Sync + Fn($($ty,)*) -> Fut,
            Fut: Send + Future<Output = Res>,
            Res: Responder,
        {

            async fn call_handler(&self, request: &mut Request) -> crate::Result<Response> {
                let ($($ty,)*) = Extractor::extract(request).await?;
                let mut response=Response::empty();
                (self)($($ty,)*).await.respond_to(request,&mut response)?;
                Ok(response)
            }
        }
    };
}

tuples!(impl_handler);

impl<H: Handler<T> + Send + Sync, T: Extractor + Send + Sync> Endpoint for IntoEndpoint<H, T> {
    fn call_endpoint<'life0, 'life1, 'async_trait>(
        &'life0 self,
        request: &'life1 mut Request,
    ) -> Pin<Box<dyn Future<Output = crate::Result<Response>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,

        Self: 'async_trait,
    {
        self.handler.call_handler(request)
    }
}
