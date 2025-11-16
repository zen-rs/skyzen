use std::{future::Future, net::SocketAddr, pin::Pin};

use crate::Endpoint;
use futures_util::{stream::MapOk, TryStreamExt};
use http_body_util::{BodyDataStream, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    service::Service,
};
use hyper_util::{rt::TokioIo, server::conn::auto::Builder as HyperBuilder};
use tokio::net::TcpListener;

type BoxedStdError = Box<dyn std::error::Error + Send + Sync + 'static>;
type BoxFuture<T> = Pin<Box<dyn Send + Sync + Future<Output = T> + 'static>>;

/// Build the Tokio runtime and serve the provided endpoint over Hyper.
///
/// # Panics
///
/// Panics if the Tokio runtime fails to initialize.
pub fn launch<Fut, E>(factory: impl FnOnce() -> Fut)
where
    Fut: Future<Output = E> + Send + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build Tokio runtime");

    runtime.block_on(async move {
        let endpoint = factory().await;
        if let Err(error) = run_server(endpoint).await {
            log::error!("Skyzen server terminated: {error}");
        }
    });
}

async fn run_server<E>(endpoint: E) -> std::io::Result<()>
where
    E: Endpoint + Clone + Send + Sync + 'static,
{
    let addr = server_addr();
    let listener = TcpListener::bind(addr).await?;
    log::info!("Skyzen listening on http://{addr}");

    loop {
        let (stream, peer) = listener.accept().await?;
        log::debug!("Accepted connection from {peer}");
        let service = IntoService::new(endpoint.clone());
        tokio::spawn(async move {
            let builder = HyperBuilder::new(hyper_util::rt::TokioExecutor::new());
            if let Err(error) = builder
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                log::error!("Hyper connection error: {error}");
            }
        });
    }
}

fn server_addr() -> SocketAddr {
    std::env::var("SKYZEN_ADDRESS")
        .unwrap_or_else(|_| "0.0.0.0:8787".to_owned())
        .parse()
        .unwrap_or_else(|error| panic!("Invalid SKYZEN_ADDRESS value: {error}"))
}

#[derive(Debug)]
struct IntoService<E> {
    endpoint: E,
}

impl<E: Endpoint + Clone> IntoService<E> {
    const fn new(endpoint: E) -> Self {
        Self { endpoint }
    }
}

impl<E: Endpoint + Send + Sync + Clone + 'static> Service<hyper::Request<Incoming>>
    for IntoService<E>
{
    type Response = hyper::Response<
        StreamBody<MapOk<crate::Body, fn(crate::utils::Bytes) -> Frame<crate::utils::Bytes>>>,
    >;
    type Error = BoxedStdError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        let mut endpoint = self.endpoint.clone();
        let fut = async move {
            let on_upgrade = hyper::upgrade::on(&mut req);
            let mut request: crate::Request =
                crate::Request::from(req.map(BodyDataStream::new).map(crate::Body::from_stream));
            request.extensions_mut().insert(on_upgrade);
            let response = endpoint.respond(&mut request).await;
            let response: Result<hyper::Response<crate::Body>, BoxedStdError> =
                response.map_err(crate::Error::into_inner);

            response.map(|response| {
                response.map(|body| {
                    let body: MapOk<
                        crate::Body,
                        fn(crate::utils::Bytes) -> Frame<crate::utils::Bytes>,
                    > = body.map_ok(Frame::data);
                    StreamBody::new(body)
                })
            })
        };

        Box::pin(fut)
    }
}
