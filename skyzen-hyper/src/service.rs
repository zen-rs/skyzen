use hyper::{server::conn::AddrStream, service::Service};
use skyzen::Endpoint;
use std::{
    convert::Infallible,
    error::Error,
    future::{ready, Future, Ready},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Debug)]
pub struct IntoMakeService<E>(Arc<E>);

impl<E> IntoMakeService<E> {
    pub fn new(endpoint: E) -> Self {
        Self(Arc::new(endpoint))
    }
}

impl<'a, E> Service<&'a AddrStream> for IntoMakeService<E>
where
    E: Endpoint + Send + Sync,
{
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;
    type Response = IntoService<E>;
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: &'a AddrStream) -> Self::Future {
        ready(Ok(IntoService::new(self.0.clone())))
    }
}

#[derive(Debug)]
pub struct IntoService<E> {
    pub endpoint: Arc<E>,
}

impl<E> IntoService<E> {
    pub fn new(endpoint: Arc<E>) -> Self {
        Self { endpoint }
    }
}

impl<E: Endpoint + Send + Sync + 'static> Service<hyper::Request<hyper::Body>> for IntoService<E> {
    type Response = hyper::Response<hyper::Body>;
    type Error = Box<dyn Error + Send + Sync + 'static>;
    type Future = Pin<Box<dyn (Future<Output = Result<Self::Response, Self::Error>>) + Send>>;
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: hyper::Request<hyper::Body>) -> Self::Future {
        let endpoint = self.endpoint.clone();

        // If `impl Trait` in associated types is stable, we will rewrite this code.
        Box::pin(async move {
            let mut request: skyzen::Request =
                skyzen::Request::from(req.map(|body| skyzen::Body::from_stream(body)));
            endpoint
                .call_endpoint(&mut request)
                .await
                .map_err(|error| error.into_inner())
                .map(|response| {
                    hyper::Response::from(response).map(|body| hyper::Body::wrap_stream(body))
                })
        })
    }
}
