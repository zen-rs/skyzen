use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll},
};

use crate::Endpoint;
use async_channel::{bounded, Receiver};
use async_executor::Executor as AsyncExecutor;
use async_net::TcpListener;
use executor_core::{try_init_global_executor, AnyExecutor, Executor as CoreExecutor, Task};
use futures_util::{future::FutureExt, stream::MapOk, StreamExt, TryStreamExt};
use http_body_util::{BodyDataStream, StreamBody};
use http_kit::{
    error::BoxHttpError,
    utils::{AsyncRead, AsyncReadExt, AsyncWrite},
    BodyError,
};
use hyper::{
    body::{Frame, Incoming},
    server::conn::{http1, http2},
    service::Service,
};
use tracing::{debug, error, info, warn};
use tracing_log::log::LevelFilter as LogLevelFilter;
use tracing_subscriber::EnvFilter;

type BoxFuture<T> = Pin<Box<dyn Send + Future<Output = T> + 'static>>;

struct HyperExecutor<E>(Arc<E>);

impl<E> Clone for HyperExecutor<E> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<E> std::fmt::Debug for HyperExecutor<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperExecutor").finish_non_exhaustive()
    }
}

impl<Fut, E> hyper::rt::Executor<Fut> for HyperExecutor<E>
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
    E: CoreExecutor + 'static,
{
    fn execute(&self, fut: Fut) {
        self.0.spawn(fut).detach();
    }
}

struct ConnectionWrapper<C>(C);

impl<C: Unpin + AsyncRead> hyper::rt::Read for ConnectionWrapper<C> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let inner = &mut self.get_mut().0;

        // SAFETY: `buf.as_mut()` gives a `&mut [MaybeUninit<u8>]` which we cast to `&mut [u8]`
        // because `AsyncRead` expects initialized memory. We advance the buffer by the number of
        // bytes written to maintain correctness.
        let buffer = unsafe { &mut *(ptr::from_mut(buf.as_mut()) as *mut [u8]) };

        match Pin::new(inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(n)) => {
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

/// Initialize the tracing subscriber + color-eyre once per process.
/// # Panics
/// If the subscriber fails to initialize.
pub fn init_logging() {
    use std::sync::Once;

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(error) = color_eyre::install() {
            eprintln!("failed to install color-eyre: {error}");
        }

        let _ = tracing_log::LogTracer::builder()
            .with_max_level(LogLevelFilter::Trace)
            .init();

        let env_filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new("info"))
            .expect("failed to build env filter");

        if tracing::dispatcher::has_been_set() {
            return;
        }

        if let Err(error) = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_target(true)
            .with_thread_ids(false)
            .with_thread_names(false)
            .with_file(false)
            .with_line_number(false)
            .event_format(
                tracing_subscriber::fmt::format()
                    .with_level(true)
                    .with_target(true)
                    .compact(),
            )
            .try_init()
        {
            // Another subscriber was already installed (likely by a test harness),
            // so we ignore the error to avoid noisy stderr output.
            tracing::debug!("tracing subscriber already initialized: {error:?}");
        }
    });
}

/// Apply CLI overrides such as `--addr` or `--port` to configure the listener.
pub fn apply_cli_overrides(args: impl IntoIterator<Item = String>) {
    let mut args = args.into_iter();
    let _ = args.next(); // binary name
    let mut listen = None;
    let mut host = None;
    let mut port = None;

    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--listen=") {
            listen = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--addr=") {
            listen = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--host=") {
            host = Some(value.to_owned());
        } else if let Some(value) = arg.strip_prefix("--port=") {
            port = Some(value.to_owned());
        } else {
            match arg.as_str() {
                "--listen" | "--addr" => {
                    if let Some(value) = args.next() {
                        listen = Some(value);
                    }
                }
                "--host" => {
                    if let Some(value) = args.next() {
                        host = Some(value);
                    }
                }
                "--port" | "-p" => {
                    if let Some(value) = args.next() {
                        port = Some(value);
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(addr) = listen {
        match addr.parse::<SocketAddr>() {
            Ok(socket) => {
                unsafe {
                    std::env::set_var("SKYZEN_ADDRESS", socket.to_string());
                }
                info!("Configured listener address via CLI: {socket}");
            }
            Err(error) => warn!("Ignoring invalid --listen address `{addr}`: {error}"),
        }
        return;
    }

    if host.is_none() && port.is_none() {
        return;
    }

    let mut candidate = server_addr();
    if let Some(host) = host {
        match host.parse::<IpAddr>() {
            Ok(ip) => candidate.set_ip(ip),
            Err(error) => {
                warn!("Ignoring invalid --host `{host}`: {error}");
                return;
            }
        }
    }
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(value) => candidate.set_port(value),
            Err(error) => {
                warn!("Ignoring invalid --port `{port}`: {error}");
                return;
            }
        }
    }

    unsafe {
        std::env::set_var("SKYZEN_ADDRESS", candidate.to_string());
    }
    info!("Configured listener address via CLI: {candidate}");
}

fn shutdown_signal() -> Receiver<()> {
    let (tx, rx) = bounded(1);
    if let Err(error) = ctrlc::set_handler(move || {
        let _ = tx.try_send(());
    }) {
        warn!("Unable to install Ctrl+C handler: {error}");
    }
    rx
}

/// Build the executor and serve the provided endpoint over Hyper.
///
/// # Panics
///
/// Panics if the global executor fails to initialize.
pub fn launch<Fut, E>(factory: impl FnOnce() -> Fut)
where
    Fut: Future<Output = E> + Send + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
{
    let executor = Arc::new(AsyncExecutor::new());
    if try_init_global_executor(executor.clone()).is_err() {
        debug!("Global executor already initialized; reusing existing instance");
    }

    let executor_clone = Arc::clone(&executor);
    async_io::block_on(executor.run(async move {
        tracing::info!("Skyzen application starting up");

        let endpoint = factory().await;
        match run_server(executor_clone, endpoint).await {
            Ok(()) => info!("Skyzen server shut down gracefully"),
            Err(error) => error!("Skyzen server terminated: {error}"),
        }
    }));
}

async fn run_server<Exec, E>(executor: Arc<Exec>, endpoint: E) -> std::io::Result<()>
where
    Exec: CoreExecutor + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
{
    const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    let listener = TcpListener::bind(server_addr()).await?;
    info!(
        "Skyzen listening on http://{}",
        listener.local_addr().unwrap()
    );

    let hyper_executor = HyperExecutor(Arc::clone(&executor));
    let shared_executor: Arc<AnyExecutor> = Arc::new(AnyExecutor::new(Arc::clone(&executor)));

    let mut incoming = listener.incoming();
    let shutdown_rx = shutdown_signal();
    let shutdown = shutdown_rx.recv().fuse();
    futures_util::pin_mut!(shutdown);

    loop {
        futures_util::select! {
            _ = shutdown => {
                info!("Ctrl+C received, stopping accept loop");
                break;
            }
            connection = incoming.next().fuse() => {
                match connection {
                    Some(Ok(stream)) => {
                        if let Ok(peer) = stream.peer_addr() {
                            debug!("Accepted connection from {peer}");
                        }
                        let endpoint = endpoint.clone();
                        let (stream, is_h2) = match sniff_protocol(stream, HTTP2_PREFACE).await {
                            Ok(result) => result,
                            Err(error) => {
                                error!("Failed to read connection preface: {error}");
                                continue;
                            }
                        };

                        if is_h2 {
                            let service = IntoService::new(endpoint, shared_executor.clone());
                            let hyper_executor = hyper_executor.clone();
                            executor
                                .spawn(async move {
                                    let builder = http2::Builder::new(hyper_executor);
                                    if let Err(error) = builder
                                        .serve_connection(ConnectionWrapper(stream), service)
                                        .await
                                    {
                                        error!("Hyper h2 connection error: {error}");
                                    }
                                })
                                .detach();
                        } else {
                            let service = IntoService::new(endpoint, shared_executor.clone());
                            executor
                                .spawn(async move {
                                    let builder = http1::Builder::new();
                                    if let Err(error) = builder
                                        .serve_connection(ConnectionWrapper(stream), service)
                                        .with_upgrades()
                                        .await
                                    {
                                        error!("Hyper h1 connection error: {error}");
                                    }
                                })
                                .detach();
                        }
                    }
                    Some(Err(error)) => error!("Accept error: {error}"),
                    None => break,
                }
            }
        }
    }

    Ok(())
}

fn server_addr() -> SocketAddr {
    std::env::var("SKYZEN_ADDRESS").map_or_else(
        |_| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        |addr| {
            // Use the provided address by default
            addr.parse()
                .unwrap_or_else(|error| panic!("Invalid SKYZEN_ADDRESS value: {error}"))
        },
    )
}

async fn sniff_protocol<C>(mut stream: C, preface: &[u8]) -> std::io::Result<(Prefixed<C>, bool)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = Vec::with_capacity(preface.len());
    while buf.len() < preface.len() {
        let remaining = preface.len() - buf.len();
        let mut chunk = vec![0u8; remaining];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        chunk.truncate(n);
        buf.extend_from_slice(&chunk);
        if !preface.starts_with(&buf) {
            return Ok((Prefixed::new(stream, buf), false));
        }
    }
    let is_h2 = buf.len() == preface.len() && buf.as_slice() == preface;
    Ok((Prefixed::new(stream, buf), is_h2))
}

#[cfg(test)]
mod tests {
    use super::sniff_protocol;
    use http_kit::utils::{AsyncRead, AsyncReadExt, AsyncWrite};
    use std::collections::VecDeque;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    struct ChunkedStream {
        chunks: VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }

    impl ChunkedStream {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: VecDeque::from(chunks),
                written: Vec::new(),
            }
        }
    }

    impl AsyncRead for ChunkedStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            match this.chunks.pop_front() {
                Some(mut chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    if n < chunk.len() {
                        chunk.drain(..n);
                        this.chunks.push_front(chunk);
                    }
                    Poll::Ready(Ok(n))
                }
                None => Poll::Ready(Ok(0)),
            }
        }
    }

    impl AsyncWrite for ChunkedStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn read_all<R: AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 16];
        loop {
            let n = reader.read(&mut buf).await.expect("read failed");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[tokio::test]
    async fn detects_split_h2_preface() {
        let chunks = vec![PREFACE[..5].to_vec(), PREFACE[5..12].to_vec(), PREFACE[12..].to_vec()];
        let stream = ChunkedStream::new(chunks);

        let (_prefixed, is_h2) = sniff_protocol(stream, PREFACE).await.unwrap();
        assert!(is_h2);
    }

    #[tokio::test]
    async fn preserves_bytes_on_mismatch() {
        let payload = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let chunks = vec![payload[..3].to_vec(), payload[3..10].to_vec(), payload[10..].to_vec()];
        let stream = ChunkedStream::new(chunks);

        let (prefixed, is_h2) = sniff_protocol(stream, PREFACE).await.unwrap();
        assert!(!is_h2);

        let restored = read_all(prefixed).await;
        assert_eq!(restored, payload);
    }
}

#[derive(Debug)]
struct IntoService<E> {
    endpoint: E,
    executor: Arc<AnyExecutor>,
}

impl<E: Endpoint + Clone> IntoService<E> {
    const fn new(endpoint: E, executor: Arc<AnyExecutor>) -> Self {
        Self { endpoint, executor }
    }
}

impl<E: Endpoint + Send + Sync + Clone + 'static> Service<hyper::Request<Incoming>>
    for IntoService<E>
{
    type Response = hyper::Response<
        StreamBody<MapOk<crate::Body, fn(crate::utils::Bytes) -> Frame<crate::utils::Bytes>>>,
    >;
    type Error = BoxHttpError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        let mut endpoint = self.endpoint.clone();
        let executor = self.executor.clone();
        let fut = async move {
            let on_upgrade = hyper::upgrade::on(&mut req);
            let method = req.method().clone();
            let path = req.uri().path().to_owned();
            let mut request: crate::Request =
                crate::Request::from(req.map(BodyDataStream::new).map(|body| {
                    crate::Body::from_stream(
                        body.map_err(|error| BodyError::Other(Box::new(error))),
                    )
                }));
            request.extensions_mut().insert(on_upgrade);
            request.extensions_mut().insert(executor);
            let response = endpoint.respond(&mut request).await;
            let response: Result<hyper::Response<crate::Body>, Self::Error> =
                response.map_err(|error| Box::new(error) as BoxHttpError);

            match &response {
                Ok(ok) => {
                    info!(
                        method = method.as_str(),
                        path = path.as_str(),
                        status = ok.status().as_u16(),
                        "request completed"
                    );
                }
                Err(err) => {
                    let status = err.status().as_u16();
                    error!(
                        method = method.as_str(),
                        path = path.as_str(),
                        status = status,
                        "request failed: {err}"
                    );
                }
            }

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
