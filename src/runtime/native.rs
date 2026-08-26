use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll},
};

use crate::{extract::PeerAddr, Endpoint};
use async_channel::{bounded, Receiver};
use async_net::TcpListener;
use executor_core::{
    smol::SmolGlobal, try_init_global_executor, AnyExecutor, Executor as CoreExecutor, Task,
};
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

        // SAFETY: `buf.as_mut()` gives a `&mut [MaybeUninit<u8>]`. `AsyncRead` expects
        // initialized memory, so we first zero every slot; only then is viewing the slice as
        // `&mut [u8]` sound (a cast without initialization would be UB). We advance the buffer
        // by the number of bytes written to maintain correctness.
        let buffer = unsafe {
            let unfilled = buf.as_mut();
            for byte in unfilled.iter_mut() {
                byte.write(0);
            }
            &mut *(ptr::from_mut(unfilled) as *mut [u8])
        };

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

/// Parse CLI overrides such as `--addr`/`--port` and return the resulting listen address.
///
/// Returns `None` when no valid override was supplied; the caller then falls back to the
/// `SKYZEN_ADDRESS` environment variable or the built-in default (see [`server_addr`]). Invalid
/// values are logged and ignored rather than mutating any global state.
#[must_use]
pub fn apply_cli_overrides(args: impl IntoIterator<Item = String>) -> Option<SocketAddr> {
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
        return match addr.parse::<SocketAddr>() {
            Ok(socket) => {
                info!("Configured listener address via CLI: {socket}");
                Some(socket)
            }
            Err(error) => {
                warn!("Ignoring invalid --listen address `{addr}`: {error}");
                None
            }
        };
    }

    if host.is_none() && port.is_none() {
        return None;
    }

    let mut candidate = server_addr();
    if let Some(host) = host {
        match host.parse::<IpAddr>() {
            Ok(ip) => candidate.set_ip(ip),
            Err(error) => {
                warn!("Ignoring invalid --host `{host}`: {error}");
                return None;
            }
        }
    }
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(value) => candidate.set_port(value),
            Err(error) => {
                warn!("Ignoring invalid --port `{port}`: {error}");
                return None;
            }
        }
    }

    info!("Configured listener address via CLI: {candidate}");
    Some(candidate)
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
pub fn launch<Fut, E>(addr: Option<SocketAddr>, factory: impl FnOnce() -> Fut)
where
    Fut: Future<Output = E> + Send + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
{
    let executor = SmolGlobal;
    if try_init_global_executor(executor).is_err() {
        debug!("Global executor already initialized; reusing existing instance");
    }

    smol::block_on(async move {
        tracing::info!("Skyzen application starting up");

        let endpoint = factory().await;
        let addr = addr.unwrap_or_else(server_addr);
        match run_server(executor, endpoint, addr).await {
            Ok(()) => info!("Skyzen server shut down gracefully"),
            Err(error) => error!("Skyzen server terminated: {error}"),
        }
    });
}

async fn run_server<Exec, E>(executor: Exec, endpoint: E, addr: SocketAddr) -> std::io::Result<()>
where
    Exec: CoreExecutor + 'static,
    E: Endpoint + Clone + Send + Sync + 'static,
{
    const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    info!("Skyzen listening on http://{}", local_addr);

    let executor = Arc::new(executor);
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
                        let peer_addr = stream.peer_addr().map_or(None, |peer| {
                            debug!("Accepted connection from {peer}");
                            Some(peer)
                        });
                        let endpoint = endpoint.clone();
                        let shared_executor = shared_executor.clone();
                        let hyper_executor = hyper_executor.clone();

                        // Spawn the per-connection task *before* sniffing the protocol preface.
                        // Awaiting client bytes inline here would let a single idle client stall
                        // the accept loop for everyone.
                        executor
                            .spawn(async move {
                                let (stream, is_h2) =
                                    match sniff_protocol_with_timeout(stream, HTTP2_PREFACE).await {
                                        Ok(result) => result,
                                        Err(error) => {
                                            error!("Failed to read connection preface: {error}");
                                            return;
                                        }
                                    };

                                let service =
                                    IntoService::new(endpoint, shared_executor, peer_addr);
                                if is_h2 {
                                    let builder = http2::Builder::new(hyper_executor);
                                    if let Err(error) = builder
                                        .serve_connection(ConnectionWrapper(stream), service)
                                        .await
                                    {
                                        error!("Hyper h2 connection error: {error}");
                                    }
                                } else {
                                    let builder = http1::Builder::new();
                                    if let Err(error) = builder
                                        .serve_connection(ConnectionWrapper(stream), service)
                                        .with_upgrades()
                                        .await
                                    {
                                        error!("Hyper h1 connection error: {error}");
                                    }
                                }
                            })
                            .detach();
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

/// How long a freshly accepted connection may take to send its protocol preface before the
/// connection is dropped. Prevents idle clients from pinning per-connection tasks forever.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run [`sniff_protocol`] with a handshake timeout so a client that connects but never sends
/// any bytes cannot hold a connection task open indefinitely.
async fn sniff_protocol_with_timeout<C>(
    stream: C,
    preface: &[u8],
) -> std::io::Result<(Prefixed<C>, bool)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    use futures_util::future::{select, Either};

    let sniff = sniff_protocol(stream, preface);
    futures_util::pin_mut!(sniff);
    let timer = async_io::Timer::after(HANDSHAKE_TIMEOUT);
    futures_util::pin_mut!(timer);

    match select(sniff, timer).await {
        Either::Left((result, _)) => result,
        Either::Right((_deadline, _)) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out waiting for connection preface",
        )),
    }
}

async fn sniff_protocol<C>(mut stream: C, preface: &[u8]) -> std::io::Result<(Prefixed<C>, bool)>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    // A single `read` may return fewer bytes than the full HTTP/2 preface (it can arrive across
    // several TCP segments). Keep reading until we have enough bytes to decide, the bytes so far
    // already diverge from the preface (so it is HTTP/1), or we hit EOF. Whatever we consumed is
    // replayed by `Prefixed`, so no data is lost regardless of the outcome.
    let mut buf = vec![0u8; preface.len()];
    let mut filled = 0;
    while filled < preface.len() {
        if buf[..filled] != preface[..filled] {
            break;
        }
        let n = stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
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
                    // Log and render through the shared helpers so every backend emits the same
                    // fields and applies the same 4xx/5xx redaction policy.
                    skyzen_core::log_endpoint_error(&err, &method, path.as_str());
                    skyzen_core::error_response(&err)
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

#[cfg(test)]
mod tests {
    use super::{apply_cli_overrides, server_addr, sniff_protocol, Prefixed};
    use http_kit::utils::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use serial_test::serial;
    use std::{
        io::{Cursor, Read},
        net::{IpAddr, Ipv4Addr, SocketAddr},
        pin::Pin,
        task::{Context, Poll},
    };

    const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    struct EnvGuard {
        original: Option<String>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            Self {
                original: std::env::var("SKYZEN_ADDRESS").ok(),
            }
        }

        fn clear() -> Self {
            let guard = Self::capture();
            unsafe {
                std::env::remove_var("SKYZEN_ADDRESS");
            }
            guard
        }

        fn set(value: &str) -> Self {
            let guard = Self::capture();
            unsafe {
                std::env::set_var("SKYZEN_ADDRESS", value);
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe {
                    std::env::set_var("SKYZEN_ADDRESS", value);
                },
                None => unsafe {
                    std::env::remove_var("SKYZEN_ADDRESS");
                },
            }
        }
    }

    #[derive(Debug, Default)]
    struct TestStream {
        read: Cursor<Vec<u8>>,
        written: Vec<u8>,
        closed: bool,
    }

    impl TestStream {
        fn new(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                read: Cursor::new(bytes.into()),
                written: Vec::new(),
                closed: false,
            }
        }
    }

    impl AsyncRead for TestStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            Poll::Ready(Read::read(&mut self.read, buf))
        }
    }

    impl AsyncWrite for TestStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, std::io::Error>> {
            self.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), std::io::Error>> {
            self.closed = true;
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    #[serial]
    fn server_addr_defaults_to_random_localhost_port() {
        let _guard = EnvGuard::clear();

        assert_eq!(
            server_addr(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        );
    }

    #[test]
    #[serial]
    fn server_addr_uses_environment_override() {
        let _guard = EnvGuard::set("127.0.0.1:4012");

        assert_eq!(
            server_addr(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4012)
        );
    }

    #[test]
    #[serial]
    fn server_addr_fast_fails_for_invalid_environment_value() {
        let _guard = EnvGuard::set("not-an-address");

        let panic = std::panic::catch_unwind(server_addr);

        assert!(panic.is_err());
    }

    #[test]
    #[serial]
    fn apply_cli_overrides_accepts_listen_aliases_and_split_flags() {
        let _guard = EnvGuard::clear();

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--addr".to_owned(),
                "127.0.0.1:5050".to_owned(),
            ]),
            Some("127.0.0.1:5050".parse().unwrap())
        );

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--host".to_owned(),
                "127.0.0.1".to_owned(),
                "-p".to_owned(),
                "6060".to_owned(),
            ]),
            Some("127.0.0.1:6060".parse().unwrap())
        );
    }

    #[test]
    #[serial]
    fn apply_cli_overrides_returns_none_for_invalid_values() {
        let _guard = EnvGuard::set("127.0.0.1:7000");

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--listen".to_owned(),
                "bad-address".to_owned(),
            ]),
            None
        );

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--host=invalid-host".to_owned(),
                "--port=7001".to_owned(),
            ]),
            None
        );

        assert_eq!(
            apply_cli_overrides([
                "skyzen".to_owned(),
                "--host=127.0.0.1".to_owned(),
                "--port=bad-port".to_owned(),
            ]),
            None
        );

        // A valid override is combined with the env-configured base address.
        assert_eq!(
            apply_cli_overrides(["skyzen".to_owned(), "--port=8080".to_owned()]),
            Some("127.0.0.1:8080".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn sniff_protocol_detects_http2_preface_and_replays_buffered_bytes() {
        let payload = [HTTP2_PREFACE, b"rest"].concat();
        let (mut stream, is_h2) = sniff_protocol(TestStream::new(payload.clone()), HTTP2_PREFACE)
            .await
            .unwrap();

        assert!(is_h2);

        let mut read = Vec::new();
        stream.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, payload);
    }

    #[tokio::test]
    async fn sniff_protocol_distinguishes_http1_and_preserves_writes() {
        let payload = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let (mut stream, is_h2) = sniff_protocol(TestStream::new(payload.clone()), HTTP2_PREFACE)
            .await
            .unwrap();

        assert!(!is_h2);

        let mut read = Vec::new();
        stream.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, payload);

        stream.write_all(b"pong").await.unwrap();
        stream.flush().await.unwrap();
        stream.close().await.unwrap();
        assert_eq!(stream.inner.written, b"pong".to_vec());
        assert!(stream.inner.closed);
    }

    #[tokio::test]
    async fn prefixed_reads_buffer_before_inner_stream() {
        let mut stream = Prefixed::new(TestStream::new(b"tail".to_vec()), b"head".to_vec());

        let mut read = Vec::new();
        stream.read_to_end(&mut read).await.unwrap();

        assert_eq!(read, b"headtail".to_vec());
    }

    // A stream that yields its bytes in caller-chosen chunks, to exercise `sniff_protocol`'s
    // short-read handling (the HTTP/2 preface arriving across multiple reads).
    struct ChunkedStream {
        chunks: std::collections::VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }

    impl ChunkedStream {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: std::collections::VecDeque::from(chunks),
                written: Vec::new(),
            }
        }
    }

    impl AsyncRead for ChunkedStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
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
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            this.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
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
    async fn sniff_detects_split_http2_preface() {
        let chunks = vec![
            HTTP2_PREFACE[..5].to_vec(),
            HTTP2_PREFACE[5..12].to_vec(),
            HTTP2_PREFACE[12..].to_vec(),
        ];
        let stream = ChunkedStream::new(chunks);

        let (_prefixed, is_h2) = sniff_protocol(stream, HTTP2_PREFACE).await.unwrap();
        assert!(is_h2);
    }

    #[tokio::test]
    async fn sniff_preserves_bytes_on_split_mismatch() {
        let payload = b"GET / HTTP/1.1\r\n\r\n".to_vec();
        let chunks = vec![
            payload[..3].to_vec(),
            payload[3..10].to_vec(),
            payload[10..].to_vec(),
        ];
        let stream = ChunkedStream::new(chunks);

        let (prefixed, is_h2) = sniff_protocol(stream, HTTP2_PREFACE).await.unwrap();
        assert!(!is_h2);

        let restored = read_all(prefixed).await;
        assert_eq!(restored, payload);
    }
}
