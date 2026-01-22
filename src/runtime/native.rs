use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll},
};

use crate::{extract::PeerAddr, Endpoint, HttpError};
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
                        let peer_addr = match stream.peer_addr() {
                            Ok(peer) => {
                                debug!("Accepted connection from {peer}");
                                Some(peer)
                            }
                            Err(_) => None,
                        };
                        let endpoint = endpoint.clone();
                        let (stream, is_h2) = match sniff_protocol(stream, HTTP2_PREFACE).await {
                            Ok(result) => result,
                            Err(error) => {
                                error!("Failed to read connection preface: {error}");
                                continue;
                            }
                        };

                        if is_h2 {
                            let service =
                                IntoService::new(endpoint, shared_executor.clone(), peer_addr);
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
                            let service =
                                IntoService::new(endpoint, shared_executor.clone(), peer_addr);
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
    let mut buf = vec![0u8; preface.len()];
    let n = stream.read(&mut buf).await?;
    buf.truncate(n);
    let is_h2 = buf.starts_with(preface);
    Ok((Prefixed::new(stream, buf), is_h2))
}

#[derive(Debug)]
struct IntoService<E> {
    endpoint: E,
    executor: Arc<AnyExecutor>,
    peer_addr: Option<SocketAddr>,
}

impl<E: Endpoint + Clone> IntoService<E> {
    const fn new(endpoint: E, executor: Arc<AnyExecutor>, peer_addr: Option<SocketAddr>) -> Self {
        Self {
            endpoint,
            executor,
            peer_addr,
        }
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
        let peer_addr = self.peer_addr;
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
            if let Some(peer_addr) = peer_addr {
                request.extensions_mut().insert(PeerAddr(peer_addr));
            }

            // Convert errors to HTTP responses at the runtime level
            let response: crate::Response = match endpoint.respond(&mut request).await {
                Ok(response) => {
                    info!(
                        method = method.as_str(),
                        path = path.as_str(),
                        status = response.status().as_u16(),
                        "request completed"
                    );
                    response
                }
                Err(err) => {
                    let status = err.status();
                    let error_message = err.to_string();

                    // For 5xx server errors, hide internal details
                    // For 4xx client errors and others, show the error message
                    let body_message = if status.is_server_error() {
                        error!(
                            method = method.as_str(),
                            path = path.as_str(),
                            status = status.as_u16(),
                            error = %error_message,
                            "internal server error"
                        );
                        "Internal server error".to_string()
                    } else {
                        warn!(
                            method = method.as_str(),
                            path = path.as_str(),
                            status = status.as_u16(),
                            error = %error_message,
                            "client error"
                        );
                        error_message
                    };

                    // Create JSON error response
                    let body = format!(
                        r#"{{"error":"{}"}}"#,
                        body_message.replace('\\', r"\\").replace('"', r#"\""#)
                    );

                    let mut response = crate::Response::new(crate::Body::from(body));
                    *response.status_mut() = status;
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        http::header::HeaderValue::from_static("application/json"),
                    );
                    response
                }
            };

            Ok(response.map(|body| {
                let body: MapOk<
                    crate::Body,
                    fn(crate::utils::Bytes) -> Frame<crate::utils::Bytes>,
                > = body.map_ok(Frame::data);
                StreamBody::new(body)
            }))
        };

        Box::pin(fut)
    }
}
