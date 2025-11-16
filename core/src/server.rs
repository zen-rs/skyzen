use core::{error::Error, future::Future};
use executor_core::Executor;
use http_kit::{
    utils::{AsyncRead, AsyncWrite, Stream},
    Endpoint,
};

/// Abstraction over HTTP server backends.
pub trait Server {
    /// Serve an [`Endpoint`] over a stream of connections.
    ///
    /// The provided `executor` runs background tasks created by the server while the `error_handler`
    /// is used to report connection-accept errors surfaced by `connectons`.
    fn serve<Fut, C, E>(
        self,
        executor: impl Executor + Send + Sync + 'static,
        error_handler: impl Fn(E) + Send + Sync + 'static,
        connectons: impl Stream<Item = Result<C, E>> + Unpin + Send + 'static,
        endpoint: impl Endpoint + Sync + Clone + 'static,
    ) -> impl Future<Output = ()>
    where
        Fut: Future + Send + 'static,
        C: Unpin + Send + AsyncRead + AsyncWrite + 'static,
        E: Error,
        Fut::Output: Send + 'static;
}
