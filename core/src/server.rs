use core::{error::Error, future::Future};
use executor_core::Executor;
use http_kit::{
    utils::{AsyncRead, AsyncWrite, Stream},
    Endpoint,
};

pub trait Server {
    fn serve<Fut, C, E>(
        self,
        executor: impl Executor,
        error_handler: impl Fn(E) + 'static,
        connectons: impl Stream<Item = Result<C, E>> + Unpin + 'static,
        endpoint: impl Endpoint + 'static + Clone,
    ) -> impl Future<Output = ()>
    where
        Fut: Future + Send + 'static,
        C: Unpin + Send + AsyncRead + AsyncWrite + 'static,
        E: Error,
        Fut::Output: Send + 'static;
}
