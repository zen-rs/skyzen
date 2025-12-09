#![warn(missing_docs, missing_debug_implementations)]

//! The hyper backend of skyzen

use core::future::Future;
use executor_core::{AnyExecutor, Executor, Task};
use http_kit::utils::{AsyncRead, AsyncReadExt, AsyncWrite, Stream, StreamExt};
use hyper::server::conn::{http1::Builder as Http1Builder, http2::Builder as Http2Builder};
use skyzen_core::{Endpoint, Server};
use std::pin::Pin;
use std::ptr;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::error;

mod service;
pub use service::IntoService;

/// Hyper-based [`Server`] implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct Hyper;

struct ExecutorWrapper<E>(Arc<E>);

impl<E> ExecutorWrapper<E> {
    const fn new(executor: Arc<E>) -> Self {
        Self(executor)
    }
}

impl<E> Clone for ExecutorWrapper<E> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Fut, E> hyper::rt::Executor<Fut> for ExecutorWrapper<E>
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
    E: executor_core::Executor + 'static,
{
    fn execute(&self, fut: Fut) {
        self.0.spawn(fut).detach();
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

#[derive(Debug)]
struct Prefixed<C> {
    buffer: Vec<u8>,
    pos: usize,
    inner: C,
}

impl<C> Prefixed<C> {
    const fn new(inner: C, buffer: Vec<u8>) -> Self {
        Self {
            buffer,
            pos: 0,
            inner,
        }
    }
}

impl<C: Unpin> Unpin for Prefixed<C> {}

impl<C: AsyncRead + Unpin> AsyncRead for Prefixed<C> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        if this.pos < this.buffer.len() {
            let available = this.buffer.len() - this.pos;
            let n = available.min(buf.len());
            buf[..n].copy_from_slice(&this.buffer[this.pos..this.pos + n]);
            this.pos += n;
            if this.pos == this.buffer.len() {
                this.buffer.clear();
                this.pos = 0;
            }
            return Poll::Ready(Ok(n));
        }

        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<C: AsyncWrite + Unpin> AsyncWrite for Prefixed<C> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

impl Server for Hyper {
    async fn serve<C, E>(
        self,
        executor: impl executor_core::Executor + 'static,
        error_handler: impl Fn(E) + Send + Sync + 'static,
        mut connections: impl Stream<Item = Result<C, E>> + Unpin + Send + 'static,
        endpoint: impl Endpoint + Sync + Clone + 'static,
    ) where
        C: Unpin + Send + AsyncRead + AsyncWrite + 'static,
        E: std::error::Error,
    {
        const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

        let executor = Arc::new(executor);
        let hyper_executor = ExecutorWrapper::new(executor.clone());
        let shared_executor: Arc<AnyExecutor> = Arc::new(AnyExecutor::new(executor.clone()));
        while let Some(connection) = connections.next().await {
            match connection {
                Ok(connection) => {
                    let serve_executor = executor.clone();
                    let endpoint = endpoint.clone();
                    let hyper_executor = hyper_executor.clone();
                    let shared_executor = shared_executor.clone();
                    let serve_future = async move {
                        let (connection, is_h2) =
                            match sniff_protocol(connection, HTTP2_PREFACE).await {
                                Ok(result) => result,
                                Err(error) => {
                                    error!("Failed to read connection preface: {error}");
                                    return;
                                }
                            };

                        if is_h2 {
                            let builder = Http2Builder::new(hyper_executor);
                            let service = IntoService::new(endpoint, shared_executor);
                            if let Err(error) = builder
                                .serve_connection(ConnectionWrapper(connection), service)
                                .await
                            {
                                error!("Failed to serve Hyper h2 connection: {error}");
                            }
                        } else {
                            let builder = Http1Builder::new();
                            let service = IntoService::new(endpoint, shared_executor);
                            if let Err(error) = builder
                                .serve_connection(ConnectionWrapper(connection), service)
                                .with_upgrades()
                                .await
                            {
                                error!("Failed to serve Hyper h1 connection: {error}");
                            }
                        }
                    };
                    serve_executor.spawn(serve_future).detach();
                }
                Err(error) => error_handler(error),
            }
        }
    }
}

async fn sniff_protocol<C>(mut stream: C, preface: &[u8]) -> std::io::Result<(Prefixed<C>, bool)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; preface.len()];
    let n = stream.read(&mut buf).await?;
    buf.truncate(n);
    let is_h2 = buf.starts_with(preface);
    Ok((Prefixed::new(stream, buf), is_h2))
}
