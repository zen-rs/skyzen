use core::{error::Error, future::Future};
use executor_core::Executor;
use http_kit::{
    utils::{AsyncRead, AsyncWrite, Stream},
    SendEndpoint,
};

/// Abstraction over HTTP server backends.
pub trait Server {
    /// Serve a [`SendEndpoint`] over a stream of connections.
    ///
    /// The provided `executor` runs background tasks created by the server while the `error_handler`
    /// is used to report connection-accept errors surfaced by `connections`. Each connection may
    /// run on any of the executor's threads, so the endpoint's future must be `Send`.
    fn serve<C, E>(
        self,
        executor: impl Executor + 'static,
        error_handler: impl Fn(E) + Send + Sync + 'static,
        connections: impl Stream<Item = Result<C, E>> + Unpin + Send + 'static,
        endpoint: impl SendEndpoint + Sync + Clone + 'static,
    ) -> impl Future<Output = ()>
    where
        C: Unpin + Send + AsyncRead + AsyncWrite + 'static,
        E: Error;
}
