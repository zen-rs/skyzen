#![warn(missing_docs, missing_debug_implementations)]

//! The hyper backend of skyzen

use executor_core::Task;
use hyper::server::conn::http1::Builder;
use skyzen::utils::{AsyncRead, StreamExt};
use skyzen::Endpoint;
use skyzen::{
    utils::{AsyncWrite, Stream},
    Server,
};
use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::error;

mod service;
/// Transform the `Endpoint` of skyzen into the `Service` of hyper
pub const fn use_hyper<E: skyzen::Endpoint + Sync + Clone>(endpoint: E) -> service::IntoService<E> {
    service::IntoService::new(endpoint)
}

/// Hyper-based [`Server`] implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct Hyper;

struct ConnectionWrapper<C>(C);

impl<C: Unpin + AsyncRead> hyper::rt::Read for ConnectionWrapper<C> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;

        // SAFETY: `buf.as_mut()` gives us a `&mut [MaybeUninit<u8>]`.
        // We must cast it to `&mut [u8]` and guarantee we will only write `n` bytes and call `advance(n)`
        let buffer = unsafe { &mut *(ptr::from_mut(buf.as_mut()) as *mut [u8]) };

        match Pin::new(inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(n)) => {
                // SAFETY: we just wrote `n` bytes into `buffer`, must now advance `n`
                unsafe {
                    buf.advance(n);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<C: AsyncWrite + Unpin> hyper::rt::Write for ConnectionWrapper<C> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let inner = &mut self.get_mut().0;
        Pin::new(inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;
        Pin::new(inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;
        Pin::new(inner).poll_close(cx)
    }
}

impl Server for Hyper {
    async fn serve<Fut, C, E>(
        self,
        executor: impl executor_core::Executor + 'static,
        error_handler: impl Fn(E) + Send + Sync + 'static,
        mut connectons: impl Stream<Item = Result<C, E>> + Unpin + Send + 'static,
        endpoint: impl Endpoint + Sync + Clone + 'static,
    ) where
        Fut: Future + Send + 'static,
        C: Unpin + Send + AsyncRead + AsyncWrite + 'static,
        E: std::error::Error,
        Fut::Output: Send + 'static,
    {
        let executor = Arc::new(executor);
        while let Some(connection) = connectons.next().await {
            match connection {
                Ok(connection) => {
                    let serve_executor = executor.clone();
                    let endpoint = endpoint.clone();
                    let serve_future = async move {
                        let builder = Builder::new();
                        let connection_future = builder
                            .serve_connection(ConnectionWrapper(connection), use_hyper(endpoint))
                            .with_upgrades();
                        connection_future
                            .await
                            .map_err(|error| error!("Failed to serve Hyper connection: {error}"))
                            .ok();
                    };
                    serve_executor.spawn(serve_future).detach();
                }
                Err(error) => error_handler(error),
            }
        }
    }
}
