use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::pin::Pin;
use http_kit::header::{HeaderMap, HeaderName, HeaderValue};
use http_kit::{
    utils::{AsyncBufRead, ByteStr, Bytes},
    Body, Request, Response, Result,
};

/// Transform a object into a part of HTTP response,always is response body,header,etc.
pub trait Responder: Send + Sync {
    /// Modify the response,sometime also read the request (but the body may have already been consumed).
    ///
    /// # Errors
    ///
    /// Returns an error if the response fails.
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<()>;
}

macro_rules! impl_tuple_responder {
        ($($ty:ident),*) => {
            #[allow(non_snake_case)]
            #[allow(unused_variables)]
            impl <$($ty:Responder,)*>Responder for ($($ty,)*){
                fn respond_to(self, request: &Request,response:&mut Response) -> Result<()>{
                    let ($($ty,)*)=self;
                    $($ty.respond_to(request,response)?;)*
                    Ok(())
                }
            }
        };
}

tuples!(impl_tuple_responder);

impl_base_responder![
    Bytes,
    Vec<u8>,
    &[u8],
    Cow<'_, [u8]>,
    Box<dyn AsyncBufRead + Send + Sync + 'static>,
    Pin<Box<dyn AsyncBufRead + Send + Sync + 'static>>,
    Body
];

impl_base_utf8_responder![ByteStr, String, &str, Cow<'_, str>];

impl Responder for Response {
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<()> {
        *response = self;
        Ok(())
    }
}

impl<T: Responder, E: Send + Sync + Into<http_kit::Error>> Responder
    for core::result::Result<T, E>
{
    fn respond_to(self, request: &Request, response: &mut Response) -> Result<()> {
        self.map_err(Into::into)
            .and_then(|responder| responder.respond_to(request, response))
    }
}

impl Responder for HeaderMap {
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<()> {
        response.headers_mut().extend(self);
        Ok(())
    }
}

impl Responder for (HeaderName, HeaderValue) {
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<()> {
        let (key, value) = self;
        response.headers_mut().append(key, value);
        Ok(())
    }
}
