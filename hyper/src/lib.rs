#![warn(missing_docs, missing_debug_implementations)]

//! The hyper backend of skyzen

use hyper_util::server::conn::auto::Builder;
use log::error;
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

mod service;
/// Transform the `Endpoint` of skyzen into the `Service` of hyper
pub const fn use_hyper<E: skyzen::Endpoint + Sync + Clone>(endpoint: E) -> service::IntoService<E> {
    service::IntoService::new(endpoint)
}

/// Hyper-based [`Server`] implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct Hyper;

#[derive(Debug)]
struct ExecutorWrapper<E>(Arc<E>);

impl<E> Clone for ExecutorWrapper<E> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Fut, E: executor_core::Executor> hyper::rt::Executor<Fut> for ExecutorWrapper<E>
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, fut: Fut) {
        drop((self.0).spawn(fut));
    }
}

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
        executor: impl executor_core::Executor + Send + Sync + 'static,
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
        let hyper_executor = ExecutorWrapper(executor.clone());
        while let Some(connection) = connectons.next().await {
            match connection {
                Ok(connection) => {
                    let serve_executor = executor.clone();
                    let builder_executor = hyper_executor.clone();
                    let endpoint = endpoint.clone();
                    let serve_future = async move {
                        let builder = Builder::new(builder_executor);
                        let connection_future = builder.serve_connection_with_upgrades(
                            ConnectionWrapper(connection),
                            use_hyper(endpoint),
                        );
                        if let Err(error) = connection_future.await {
                            error!("Failed to serve Hyper connection: {error}");
                        }
                    };
                    drop(serve_executor.spawn(serve_future));
                }
                Err(error) => error_handler(error),
            }
        }
    }
}
