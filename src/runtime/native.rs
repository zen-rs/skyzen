use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

use crate::Endpoint;
use futures_util::{stream::MapOk, TryStreamExt};
use http_body_util::{BodyDataStream, StreamBody};
use http_kit::{BodyError, error::BoxHttpError};
use hyper::{
    body::{Frame, Incoming},
    service::Service,
};
use hyper_util::{rt::TokioIo, server::conn::auto::Builder as HyperBuilder};
use tokio::{net::TcpListener, signal};
use tracing_subscriber::EnvFilter;

type BoxFuture<T> = Pin<Box<dyn Send + Future<Output = T> + 'static>>;

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
            .with_max_level(log::LevelFilter::Trace)
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
                std::env::set_var("SKYZEN_ADDRESS", socket.to_string());
                log::info!("Configured listener address via CLI: {socket}");
            }
            Err(error) => log::warn!("Ignoring invalid --listen address `{addr}`: {error}"),
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
                log::warn!("Ignoring invalid --host `{host}`: {error}");
                return;
            }
        }
    }
    if let Some(port) = port {
        match port.parse::<u16>() {
            Ok(value) => candidate.set_port(value),
            Err(error) => {
                log::warn!("Ignoring invalid --port `{port}`: {error}");
                return;
            }
        }
    }

    std::env::set_var("SKYZEN_ADDRESS", candidate.to_string());
    log::info!("Configured listener address via CLI: {candidate}");
}

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
        match run_server(endpoint).await {
            Ok(()) => log::info!("Skyzen server shut down gracefully"),
            Err(error) => log::error!("Skyzen server terminated: {error}"),
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

    let shutdown_signal = signal::ctrl_c();
    tokio::pin!(shutdown_signal);

    loop {
        tokio::select! {
            biased;
            _ = shutdown_signal.as_mut() => {
                log::info!("Ctrl+C received, stopping accept loop");
                break;
            }
            accept_result = listener.accept() => {
                let (stream, peer) = accept_result?;
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
    }

    Ok(())
}

fn server_addr() -> SocketAddr {
    std::env::var("SKYZEN_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_owned())
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
    type Error = BoxHttpError;
    type Future = BoxFuture<Result<Self::Response, Self::Error>>;

    fn call(&self, mut req: hyper::Request<Incoming>) -> Self::Future {
        let mut endpoint = self.endpoint.clone();
        let fut = async move {
            let on_upgrade = hyper::upgrade::on(&mut req);
            let mut request: crate::Request =
                crate::Request::from(req.map(BodyDataStream::new).map(|body| {
                    crate::Body::from_stream(body.map_err(|error|{
                        BodyError::Other(Box::new(error))
                    }))
                }));
            request.extensions_mut().insert(on_upgrade);
            let response = endpoint.respond(&mut request).await;
            let response: Result<hyper::Response<crate::Body>, Self::Error> =
                response.map_err(|error| Box::new(error) as BoxHttpError);

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
