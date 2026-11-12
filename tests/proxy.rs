//! The reverse proxy end to end: a client, the proxy and an origin, each over real loopback
//! sockets on one compio runtime — the runtime `#[skyzen::main]` serves every thread with.

use std::{
    cell::RefCell,
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    rc::Rc,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http_kit::BodyFrame;
use http_kit_io::{compio::Compio, Dial as _, FromStd as _};
use ipnet::IpNet;
use skyzen::{
    extract::TrustedProxies,
    header::{self, HeaderMap, HeaderValue},
    proxy::{Proxy, Upstream},
    routing::{CreateRouteNode, Route},
    runtime::ServingThread,
    Body, Endpoint, Method, Request, Response, StatusCode,
};
use skyzen_h1::H1;

/// A request body in Chinese, Japanese and Korean.
const ORDER: &str = "订单：一杯茶。注文：お茶を一杯。주문: 차 한 잔.";
/// What the origin refuses an upgrade with.
const REFUSAL: &str = "不升级 / アップグレードしません / 업그레이드 안 함";

/// One chunk of the bodies the backpressure tests stream.
const CHUNK: usize = 64 * 1024;
/// How many chunks those bodies have: 64 MiB in all.
const CHUNKS: usize = 1024;
/// The most a stalled exchange may have produced past what its reader took. Socket buffers on
/// both legs account for a few MiB; a proxy that held the body whole would reach all 64.
const IN_FLIGHT_BOUND: usize = 16 * 1024 * 1024;

/// How many times [`ORDER`] repeats in the bodies the splice tests send: about 4 MiB, far more
/// than one read past a message head can buffer, so most of it is still in the socket when its
/// consumer asks for it.
const BULK_REPEATS: usize = 64 * 1024;

/// The body the splice tests send through the proxy.
fn bulk() -> Vec<u8> {
    ORDER.repeat(BULK_REPEATS).into_bytes()
}

thread_local! {
    /// What this thread logged since [`run_logged`] started capturing it; `None` while nothing
    /// captures.
    static CAPTURED: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// Writes each event into the logging thread's [`CAPTURED`] buffer, or nowhere.
struct ThreadLog;

impl std::io::Write for ThreadLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURED.with_borrow_mut(|captured| {
            if let Some(captured) = captured {
                captured.extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`run`] the scenario, and return what its thread logged, at every level.
///
/// One subscriber serves the whole test binary, installed by the first capture. A subscriber
/// scoped to each test's thread would race the others on tracing's process-wide callsite cache,
/// which can disable a callsite for every thread while one test swaps its subscriber.
fn run_logged<F: Future>(scenario: F) -> (F::Output, String) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(|| ThreadLog)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the one subscriber");
    });
    CAPTURED.set(Some(Vec::new()));
    let output = run(scenario);
    let log = CAPTURED.take().expect("the capture this call started");
    (output, String::from_utf8(log).expect("a UTF-8 log"))
}

/// The bytes the HTTP/1.1 writers in `log` spliced from a pipe into their socket: `http-kit-h1`
/// records `sending a pipe` with its length exactly where it does.
fn spliced(log: &str) -> usize {
    log.lines()
        .filter(|line| line.contains("sending a pipe"))
        .map(|line| {
            let (_, len) = line.rsplit_once("len=").expect("a pipe's length");
            len.trim().parse::<usize>().expect("a numeric length")
        })
        .sum()
}

/// What the origin received.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    uri: String,
    version: http::Version,
    headers: HeaderMap,
    body: Vec<u8>,
    trailers: Option<HeaderMap>,
}

/// The origin the proxy forwards to.
#[derive(Clone)]
struct Origin {
    seen: Rc<RefCell<Vec<Seen>>>,
    /// Lets `/v1/slow-reader` read past its first frame.
    release: async_channel::Receiver<()>,
    /// What `/v1/firehose` has produced so far.
    produced: Arc<AtomicUsize>,
}

/// Every frame of `body`: its data, and its trailers if it ends with any.
async fn read_frames(mut body: Body) -> (Vec<u8>, Option<HeaderMap>) {
    let mut data = Vec::new();
    let mut trailers = None;
    while let Some(frame) = body.next().await {
        match frame.expect("body frame") {
            BodyFrame::Data(bytes) => data.extend_from_slice(&bytes),
            BodyFrame::Trailers(fields) => trailers = Some(fields),
            other => panic!("unexpected frame {other:?}"),
        }
    }
    (data, trailers)
}

/// A body of [`CHUNKS`] chunks that counts each chunk as it is pulled, so `produced` shows how far
/// its reader has asked.
fn counted_body(produced: Arc<AtomicUsize>) -> Body {
    let chunk = Bytes::from(vec![b'x'; CHUNK]);
    Body::from_frames(stream::iter(0..CHUNKS).map(move |_| {
        produced.fetch_add(chunk.len(), Ordering::SeqCst);
        Ok::<_, std::io::Error>(BodyFrame::Data(chunk.clone()))
    }))
}

impl Endpoint for Origin {
    type Error = Infallible;

    async fn respond(&mut self, request: &mut Request) -> Result<Response, Infallible> {
        let path = request.uri().path().to_owned();
        let body = request.body_mut().take().expect("body untaken");
        match path.as_str() {
            "/v1/echo" => {
                let (data, trailers) = read_frames(body).await;
                self.record(request, data.clone(), trailers);
                let mut returned = HeaderMap::new();
                returned.insert("x-origin-digest", HeaderValue::from_static("cafe"));
                let mut response = Response::new(Body::from_frames(stream::iter([
                    Ok::<_, Infallible>(BodyFrame::Data(Bytes::from(data))),
                    Ok(BodyFrame::Trailers(returned)),
                ])));
                let fields = response.headers_mut();
                fields.insert(header::CONNECTION, HeaderValue::from_static("x-origin-hop"));
                fields.insert("x-origin-hop", HeaderValue::from_static("1"));
                fields.insert("keep-alive", HeaderValue::from_static("timeout=5"));
                Ok(response)
            }
            "/v1/missing" => {
                self.record(request, Vec::new(), None);
                let mut response = Response::new(Body::from("没有 / ありません / 없음"));
                *response.status_mut() = StatusCode::NOT_FOUND;
                Ok(response)
            }
            "/v1/slow-reader" => {
                let mut body = body;
                let mut taken = 0;
                if let Some(frame) = body.next().await {
                    taken += frame.expect("first frame").as_data().map_or(0, Bytes::len);
                }
                self.release.recv().await.expect("released");
                while let Some(frame) = body.next().await {
                    taken += frame.expect("frame").as_data().map_or(0, Bytes::len);
                }
                Ok(Response::new(Body::from(taken.to_string())))
            }
            "/v1/firehose" => Ok(Response::new(counted_body(Arc::clone(&self.produced)))),
            "/v1/upload" => {
                let (data, trailers) = read_frames(body).await;
                self.record(request, data, trailers);
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::NO_CONTENT;
                Ok(response)
            }
            "/v1/download" => Ok(Response::new(Body::from(bulk()))),
            "/v1/refuse-upgrade" => {
                self.record(request, Vec::new(), None);
                let mut response = Response::new(Body::from(REFUSAL));
                *response.status_mut() = StatusCode::BAD_REQUEST;
                Ok(response)
            }
            other => panic!("the origin has no {other}"),
        }
    }
}

impl Origin {
    /// An origin that has seen nothing, and the sender that releases its `/v1/slow-reader`.
    fn new() -> (Self, async_channel::Sender<()>) {
        let (release, released) = async_channel::unbounded();
        let origin = Self {
            seen: Rc::default(),
            release: released,
            produced: Arc::default(),
        };
        (origin, release)
    }

    fn record(&self, request: &Request, body: Vec<u8>, trailers: Option<HeaderMap>) {
        self.seen.borrow_mut().push(Seen {
            method: request.method().clone(),
            uri: request.uri().to_string(),
            version: request.version(),
            headers: request.headers().clone(),
            body,
            trailers,
        });
    }

    fn last(&self) -> Seen {
        self.seen
            .borrow()
            .last()
            .cloned()
            .expect("the origin saw a request")
    }
}

/// Serve `endpoint` with `H1` on a loopback listener of the current runtime.
fn serve<E: Endpoint + Clone + 'static>(endpoint: E) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local address");
    let io = Compio::default();
    let listener = io.tcp_listener(listener).expect("register listener");
    compio::runtime::spawn(async move {
        H1::default()
            .serve_listener(
                io,
                compio::runtime::Runtime::current(),
                listener,
                endpoint,
                core::future::pending(),
            )
            .await;
    })
    .detach();
    addr
}

/// The current runtime's thread, as a host that runs it hands an endpoint factory: a compio
/// thread whose ring registers no region.
fn thread() -> ServingThread {
    use futures_util::FutureExt as _;

    // A ring asked for no region registers nothing, so this resolves without suspending.
    let ring = Compio::default()
        .register_fixed_region()
        .now_or_never()
        .expect("a ring asked for no region registers at once")
        .expect("a ring asked for no region registers none");
    ServingThread::on_ring(ring)
}

/// The scenario's three parties.
struct Stage {
    origin: Origin,
    proxy: SocketAddr,
    release: async_channel::Sender<()>,
}

/// An origin, and a proxy mounted at `/api` forwarding to it under `/v1`, with `configure` applied
/// to the proxy's route.
fn stage(configure: impl FnOnce(Route) -> Route) -> Stage {
    let (origin, release) = Origin::new();
    let origin_addr = serve(origin.clone());
    let target = format!("http://{origin_addr}/v1")
        .parse()
        .expect("upstream URI");
    let upstream = Upstream::new(&target).expect("upstream");
    let router = configure(Route::new(
        ("/api".mount(Proxy::new(&upstream, &thread())),),
    ))
    .build();
    Stage {
        origin,
        proxy: serve(router),
        release,
    }
}

/// A fresh HTTP/1.1 connection to `addr`.
async fn connect(addr: SocketAddr) -> http_kit_h1::SendRequest {
    let io = Compio::default();
    let stream = io.connect(addr).await.expect("connect");
    let (send, connection) =
        http_kit_h1::handshake_stream(stream, io, http_kit_h1::ClientConfig::default());
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    send
}

fn request(method: Method, path: &str, body: Body) -> Request {
    let mut request = Request::new(body);
    *request.method_mut() = method;
    *request.uri_mut() = path.parse().expect("path");
    request
        .headers_mut()
        .insert(header::HOST, HeaderValue::from_static("shop.example"));
    request
}

fn run<F: Future>(scenario: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(scenario)
}

/// Wait until `counter` stops moving, and return where it stopped.
async fn settle(counter: &AtomicUsize) -> usize {
    let mut last = counter.load(Ordering::SeqCst);
    let mut unchanged = 0;
    for _ in 0..400 {
        compio::time::sleep(Duration::from_millis(25)).await;
        let now = counter.load(Ordering::SeqCst);
        if now == last && now > 0 {
            unchanged += 1;
            if unchanged == 8 {
                return now;
            }
        } else {
            unchanged = 0;
            last = now;
        }
    }
    panic!("the producer never stalled; it reached {last} bytes");
}

#[test]
fn h1_to_h1_streams_bodies_and_trailers_both_ways() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;

        let mut trailers = HeaderMap::new();
        trailers.insert("x-order-checksum", HeaderValue::from_static("1f2e"));
        let body = Body::from_frames(stream::iter([
            Ok::<_, Infallible>(BodyFrame::Data(Bytes::from_static(ORDER.as_bytes()))),
            Ok(BodyFrame::Trailers(trailers)),
        ]));
        let mut request = request(Method::POST, "/api/echo?table=7", body);
        request
            .headers_mut()
            .insert(header::TE, HeaderValue::from_static("trailers"));
        let response = send.send(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("x-origin-hop").is_none());
        assert!(response.headers().get("keep-alive").is_none());
        assert_eq!(response.headers()[header::VIA], "1.1 skyzen");
        let (data, returned) = read_frames(response.into_body()).await;
        assert_eq!(data, ORDER.as_bytes());
        assert_eq!(
            returned.expect("response trailers")["x-origin-digest"],
            "cafe"
        );

        let seen = stage.origin.last();
        assert_eq!(seen.method, Method::POST);
        assert_eq!(seen.uri, "/v1/echo?table=7");
        assert_eq!(seen.version, http::Version::HTTP_11);
        assert_eq!(seen.body, ORDER.as_bytes());
        assert_eq!(
            seen.trailers.expect("request trailers")["x-order-checksum"],
            "1f2e"
        );
        assert_eq!(seen.headers[header::TE], "trailers");
    });
}

#[test]
fn response_trailers_a_client_did_not_accept_fail_the_response() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let response = send
            .send(request(
                Method::POST,
                "/api/echo",
                Body::from(ORDER.as_bytes()),
            ))
            .await
            .expect("response head");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let mut failed = false;
        while let Some(frame) = body.next().await {
            match frame {
                Ok(BodyFrame::Trailers(_)) => panic!("trailers reached a client that refused them"),
                Ok(_) => {}
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        assert!(
            failed,
            "the response must end in an error, not quietly lose its trailers"
        );
        assert!(stage.origin.last().headers.get(header::TE).is_none());
    });
}

#[test]
fn hop_by_hop_fields_are_removed_and_host_names_the_upstream() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let mut request = request(Method::POST, "/api/echo", Body::from(ORDER.as_bytes()));
        let fields = request.headers_mut();
        fields.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-client-hop"),
        );
        fields.insert("x-client-hop", HeaderValue::from_static("1"));
        fields.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        fields.insert("proxy-connection", HeaderValue::from_static("keep-alive"));
        fields.insert(header::TE, HeaderValue::from_static("trailers"));
        fields.insert(
            header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("zh-CN, ja-JP, ko-KR"),
        );
        let response = send.send(request).await.expect("response");
        drop(read_frames(response.into_body()).await);

        let seen = stage.origin.last();
        for hop in ["x-client-hop", "keep-alive", "proxy-connection"] {
            assert!(seen.headers.get(hop).is_none(), "{hop} reached the origin");
        }
        // The only option left is the proxy's own: its `TE` is hop-by-hop too.
        assert_eq!(seen.headers[header::CONNECTION], "te");
        assert_eq!(seen.headers[header::ACCEPT_LANGUAGE], "zh-CN, ja-JP, ko-KR");
        let host = seen.headers[header::HOST].to_str().expect("host");
        assert!(host.starts_with("127.0.0.1:"), "Host is {host}");
    });
}

#[test]
fn an_untrusted_peer_has_its_forwarding_fields_replaced() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let mut request = request(Method::GET, "/api/echo", Body::empty());
        let fields = request.headers_mut();
        fields.insert(
            header::FORWARDED,
            HeaderValue::from_static("for=198.51.100.1"),
        );
        fields.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1"));
        fields.insert("x-real-ip", HeaderValue::from_static("198.51.100.1"));
        fields.insert(header::VIA, HeaderValue::from_static("1.1 edge"));
        fields.insert(header::TE, HeaderValue::from_static("trailers"));
        let response = send.send(request).await.expect("response");
        drop(read_frames(response.into_body()).await);

        let seen = stage.origin.last();
        let forwarded: Vec<_> = seen.headers.get_all(header::FORWARDED).iter().collect();
        assert_eq!(forwarded, ["for=127.0.0.1;host=shop.example"]);
        assert!(seen.headers.get("x-forwarded-for").is_none());
        assert!(seen.headers.get("x-real-ip").is_none());
        // `Via` records protocol hops, not identities: it is extended whoever sent it.
        let via: Vec<_> = seen.headers.get_all(header::VIA).iter().collect();
        assert_eq!(via, ["1.1 edge", "1.1 skyzen"]);
    });
}

#[test]
fn a_trusted_peer_has_its_forwarding_chain_extended() {
    run(async {
        let loopback = IpNet::from_str("127.0.0.0/8").expect("net");
        let stage = stage(|route| route.layer(TrustedProxies::new([loopback])));
        let send = connect(stage.proxy).await;
        let mut request = request(Method::GET, "/api/echo", Body::empty());
        let fields = request.headers_mut();
        fields.insert(
            header::FORWARDED,
            HeaderValue::from_static("for=198.51.100.1;proto=https"),
        );
        fields.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1"));
        fields.insert(header::TE, HeaderValue::from_static("trailers"));
        let response = send.send(request).await.expect("response");
        drop(read_frames(response.into_body()).await);

        let seen = stage.origin.last();
        let forwarded: Vec<_> = seen.headers.get_all(header::FORWARDED).iter().collect();
        assert_eq!(
            forwarded,
            [
                "for=198.51.100.1;proto=https",
                "for=127.0.0.1;host=shop.example"
            ]
        );
        let xff: Vec<_> = seen.headers.get_all("x-forwarded-for").iter().collect();
        assert_eq!(xff, ["198.51.100.1", "127.0.0.1"]);
    });
}

#[test]
fn an_error_status_from_the_origin_is_the_answer() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let response = send
            .send(request(Method::GET, "/api/missing", Body::empty()))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let (data, _) = read_frames(response.into_body()).await;
        assert_eq!(data, "没有 / ありません / 없음".as_bytes());
    });
}

#[test]
fn an_unreachable_origin_is_a_bad_gateway() {
    run(async {
        // A port nothing listens on: bound, then released.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("local address");
        let target = format!("http://{closed}").parse().expect("URI");
        let upstream = Upstream::new(&target).expect("upstream");
        let proxy = serve(Route::new(("/".mount(Proxy::new(&upstream, &thread())),)).build());
        let send = connect(proxy).await;
        let response = send
            .send(request(Method::GET, "/anything", Body::empty()))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    });
}

#[test]
fn a_refused_upgrade_is_passed_through_unchanged() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let mut request = request(Method::GET, "/api/refuse-upgrade", Body::empty());
        let fields = request.headers_mut();
        fields.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        fields.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        let response = send.send(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get(header::UPGRADE).is_none());
        assert_eq!(response.headers()[header::VIA], "1.1 skyzen");
        let (data, _) = read_frames(response.into_body()).await;
        assert_eq!(data, REFUSAL.as_bytes());

        // The offer reached the origin: `Upgrade`, and a `Connection` nominating it.
        let seen = stage.origin.last();
        assert_eq!(seen.headers[header::UPGRADE], "websocket");
        assert_eq!(seen.headers[header::CONNECTION], "upgrade");
    });
}

#[test]
fn a_switch_the_client_did_not_ask_for_is_a_bad_gateway() {
    run(async {
        // An origin that switches protocols whatever it is asked.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let origin = listener.local_addr().expect("local address");
        let switcher = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener.accept().expect("accept");
            let mut head = Vec::new();
            let mut byte = [0; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).expect("request head");
                head.push(byte[0]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\n\
                      Upgrade: websocket\r\n\r\n",
                )
                .expect("write the switch");
            String::from_utf8(head).expect("an ASCII head")
        });

        let target = format!("http://{origin}").parse().expect("URI");
        let upstream = Upstream::new(&target).expect("upstream");
        let proxy = serve(Route::new(("/".mount(Proxy::new(&upstream, &thread())),)).build());
        let send = connect(proxy).await;
        let response = send
            .send(request(Method::GET, "/chat", Body::empty()))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let head = switcher.join().expect("the origin thread").to_lowercase();
        assert!(
            !head.contains("upgrade:"),
            "the proxy offered a switch:\n{head}"
        );
    });
}

/// The WebSocket tunnel: a WebSocket origin behind the proxy, and a client upgrading through it.
#[cfg(feature = "ws")]
mod websocket {
    use std::time::Duration;

    use async_tungstenite::{
        client_async,
        tungstenite::{protocol::CloseFrame, Message},
        WebSocketStream,
    };
    use bytes::Bytes;
    use futures_util::StreamExt as _;
    use http_kit_io::{
        compio::{Compio, TcpStream},
        poll::CopyingPoll,
        Dial as _, IntoPoll as _,
    };
    use skyzen::{
        proxy::{Proxy, Upstream},
        routing::{CreateRouteNode, Route, Router},
        websocket::WebSocketMessage,
    };

    use super::{run, run_logged, serve, thread};

    /// The text messages: Chinese, Japanese and Korean.
    pub const GREETINGS: [&str; 3] = ["你好世界", "こんにちは", "안녕하세요"];
    /// The message that makes the origin close the connection itself.
    const FAREWELL: &str = "再见 / さようなら / 안녕히 가세요";

    type Client = WebSocketStream<CopyingPoll<TcpStream>>;

    /// A WebSocket origin echoing every message at `/v1/echo` until it receives [`FAREWELL`].
    pub fn echo_origin() -> Router {
        Route::new(("/v1/echo".ws(|mut socket| async move {
            while let Some(Ok(message)) = socket.next().await {
                let sent = match message {
                    WebSocketMessage::Text(text) if &*text == FAREWELL => break,
                    WebSocketMessage::Text(text) => socket.send_text(text).await,
                    WebSocketMessage::Binary(data) => socket.send_binary(data).await,
                    _ => Ok(()),
                };
                if sent.is_err() {
                    return;
                }
            }
            let _ = socket.close(None).await;
        }),))
        .build()
    }

    /// A WebSocket echo origin, and a proxy mounted at `/api` forwarding to it under `/v1`;
    /// returns the client's connection through the proxy, and the `Via` of its `101`.
    async fn connect() -> (Client, String) {
        let origin = serve(echo_origin());
        let target = format!("http://{origin}/v1").parse().expect("upstream URI");
        let upstream = Upstream::new(&target).expect("upstream");
        let proxy = serve(Route::new(("/api".mount(Proxy::new(&upstream, &thread())),)).build());

        let stream = Compio::default().connect(proxy).await.expect("connect");
        let (client, response) = client_async("ws://shop.example/api/echo", stream.into_poll())
            .await
            .expect("the WebSocket handshake through the proxy");
        assert_eq!(response.status(), 101);
        let via = response.headers()["via"].to_str().expect("Via").to_owned();
        (client, via)
    }

    /// The next message from the origin, within a bound a working tunnel never comes near.
    async fn next(client: &mut Client) -> Option<Message> {
        compio::time::timeout(Duration::from_secs(10), client.next())
            .await
            .expect("the tunnel stalled")
            .map(|message| message.expect("a WebSocket frame"))
    }

    /// Messages cross the tunnel both ways, which on Linux splices the two connections together
    /// and elsewhere copies between them.
    #[test]
    fn text_and_binary_messages_round_trip_through_the_tunnel() {
        let ((), log) = run_logged(async {
            let (mut client, via) = connect().await;
            assert_eq!(via, "1.1 skyzen");

            for greeting in GREETINGS {
                client.send(Message::text(greeting)).await.expect("send");
                let echoed = next(&mut client).await.expect("an echo");
                assert_eq!(echoed.into_text().expect("text").as_str(), greeting);
            }
            // Every byte value, so nothing on the way treats the payload as text.
            let binary: Vec<u8> = GREETINGS
                .concat()
                .into_bytes()
                .into_iter()
                .chain(0..=u8::MAX)
                .collect();
            client
                .send(Message::binary(Bytes::from(binary.clone())))
                .await
                .expect("send");
            assert_eq!(
                next(&mut client).await.expect("an echo"),
                Message::binary(binary)
            );

            // The client closes: the origin answers its close frame, and the tunnel ends.
            client.close(None).await.expect("close");
            while let Some(message) = next(&mut client).await {
                assert!(message.is_close(), "after the close: {message:?}");
            }
        });
        // The proxy records the path before the first byte crosses.
        let path = if cfg!(any(target_os = "linux", target_os = "android")) {
            "splicing a proxied upgrade"
        } else {
            "copying a proxied upgrade"
        };
        assert!(log.contains(path), "the tunnel did not record {path:?}");
    }

    #[test]
    fn an_origin_close_reaches_the_client() {
        run(async {
            let (mut client, _) = connect().await;
            client.send(Message::text(FAREWELL)).await.expect("send");
            let closed = next(&mut client).await.expect("the origin's close frame");
            assert!(
                matches!(closed, Message::Close(None | Some(CloseFrame { .. }))),
                "{closed:?}"
            );
            assert!(
                next(&mut client).await.is_none(),
                "the stream ends after the close"
            );
        });
    }
}

/// On Linux most of a body of `len` bytes went through pipes, the rest having been read with a
/// message head; elsewhere none did.
fn assert_spliced(log: &str, len: usize) {
    let spliced = spliced(log);
    if cfg!(any(target_os = "linux", target_os = "android")) {
        assert!(
            spliced >= len / 2 && spliced <= len,
            "{spliced} of the body's {len} bytes were spliced"
        );
    } else {
        assert_eq!(spliced, 0, "a platform without pipes spliced");
    }
}

/// A request body goes from the client's socket into the upstream's through pipes. The response
/// has no body, so every pipe the log shows is the request's.
#[test]
fn an_upload_is_spliced_upstream() {
    let ((), log) = run_logged(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let response = send
            .send(request(Method::PUT, "/api/upload", Body::from(bulk())))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            stage.origin.last().body == bulk(),
            "the upload arrived changed"
        );
    });
    assert_spliced(&log, bulk().len());
}

/// A response body goes from the upstream's socket into the client's through pipes. The request
/// has no body, so every pipe the log shows is the response's.
#[test]
fn a_download_is_spliced_to_the_client() {
    let ((), log) = run_logged(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let response = send
            .send(request(Method::GET, "/api/download", Body::empty()))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let (data, _) = read_frames(response.into_body()).await;
        assert!(data == bulk(), "the download arrived changed");
    });
    assert_spliced(&log, bulk().len());
}

#[test]
fn a_slow_origin_paces_the_client_upload() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let produced = Arc::new(AtomicUsize::new(0));
        let body = counted_body(Arc::clone(&produced));
        let exchange = compio::runtime::spawn(async move {
            send.send(request(Method::PUT, "/api/slow-reader", body))
                .await
                .expect("response")
        });

        let stalled = settle(&produced).await;
        assert!(
            stalled <= IN_FLIGHT_BOUND,
            "the client wrote {stalled} bytes while the origin had read one chunk"
        );

        stage.release.send(()).await.expect("release the origin");
        let response = exchange.await.expect("exchange task");
        assert_eq!(response.status(), StatusCode::OK);
        let (data, _) = read_frames(response.into_body()).await;
        assert_eq!(data, (CHUNK * CHUNKS).to_string().as_bytes());
    });
}

#[test]
fn a_slow_client_paces_the_origin_download() {
    run(async {
        let stage = stage(|route| route);
        let send = connect(stage.proxy).await;
        let response = send
            .send(request(Method::GET, "/api/firehose", Body::empty()))
            .await
            .expect("response");
        let mut body = response.into_body();
        let mut read = body
            .next()
            .await
            .expect("a first frame")
            .expect("frame")
            .as_data()
            .map_or(0, Bytes::len);

        let stalled = settle(&stage.origin.produced).await;
        assert!(
            stalled <= read + IN_FLIGHT_BOUND,
            "the origin produced {stalled} bytes while the client had read {read}"
        );

        while let Some(frame) = body.next().await {
            read += frame.expect("frame").as_data().map_or(0, Bytes::len);
        }
        assert_eq!(read, CHUNK * CHUNKS);
    });
}

/// The proxy between two kernel TLS connections (Linux, with the kernel's `tls` module loaded, as
/// CI loads it): its bodies and its tunnel splice exactly as they do between plain sockets, the
/// records sealed and opened by the kernel on both sides.
#[cfg(all(target_os = "linux", feature = "server-tls"))]
mod kernel_tls {
    use std::{cell::RefCell, net::SocketAddr, rc::Rc, sync::Arc};

    use http_kit_io::{
        compio::{Compio, TcpStream},
        Dial as _, FromStd as _, Listener as _,
    };
    use http_kit_tls::{
        rustls::{
            self,
            pki_types::{CertificateDer, PrivateKeyDer, ServerName},
        },
        Client, KernelTls, Mode, TlsAcceptor, TlsConnector, TlsStream,
    };
    use skyzen::{
        proxy::{transport, Proxy, Transport, Upstream},
        routing::{CreateRouteNode, Route},
        Body, Endpoint, Method, StatusCode,
    };
    use skyzen_h1::H1;

    use super::{assert_spliced, bulk, read_frames, request, run_logged, thread, Origin};

    /// The record-layer mode of every connection a TLS server accepted.
    type Modes = Rc<RefCell<Vec<Mode>>>;

    /// A self-signed `localhost` identity: an acceptor presenting it, which hands every
    /// connection's records to the kernel, and the certificate for a client to trust.
    fn identity() -> (TlsAcceptor, Vec<u8>) {
        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("rcgen");
        let der = certified.cert.der().to_vec();
        let key =
            PrivateKeyDer::try_from(certified.signing_key.serialize_der()).expect("a PKCS#8 key");
        let config = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("default protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(der.clone())], key)
        .expect("the generated pair");
        let acceptor = TlsAcceptor::new(Arc::new(config))
            .with_kernel_tls(KernelTls::probe().expect("the kernel probes"));
        (acceptor, der)
    }

    /// Serve `endpoint` with `H1` over TLS on a loopback listener of the current runtime: its
    /// address, the modes its connections settled on, and the certificate to trust.
    fn serve_tls<E: Endpoint + Clone + 'static>(endpoint: E) -> (SocketAddr, Modes, Vec<u8>) {
        let (acceptor, der) = identity();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local address");
        let io = Compio::default();
        let listener = io.tcp_listener(listener).expect("register listener");
        let modes = Modes::default();
        let accepted = Rc::clone(&modes);
        compio::runtime::spawn(async move {
            while let Ok((stream, peer)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let endpoint = endpoint.clone();
                let accepted = Rc::clone(&accepted);
                let io = io.clone();
                compio::runtime::spawn(async move {
                    // Boxed: with kernel TLS state the handshake's future is over 16 KiB.
                    let stream = Box::pin(acceptor.accept(stream, &io))
                        .await
                        .expect("the server handshake");
                    accepted.borrow_mut().push(stream.negotiated().mode());
                    H1::default()
                        .serve_connection(
                            io,
                            stream,
                            Some(peer),
                            endpoint,
                            core::future::pending(),
                            (),
                        )
                        .await;
                })
                .detach();
            }
        })
        .detach();
        (addr, modes, der)
    }

    /// A proxy over TLS, mounted at `/api`, forwarding to the TLS `origin` under `/v1`, whose
    /// certificate is `origin_der`.
    fn proxy_to(origin: SocketAddr, origin_der: Vec<u8>) -> (SocketAddr, Modes, Vec<u8>) {
        let transport = Transport::builder()
            .proxy(transport::Proxy::none())
            .extra_root_certificate_der(origin_der)
            .build()
            .expect("the upstream transport");
        let target = format!("https://localhost:{}/v1", origin.port())
            .parse()
            .expect("upstream URI");
        let upstream = Upstream::new(&target)
            .expect("upstream")
            .transport(transport);
        serve_tls(Route::new(("/api".mount(Proxy::new(&upstream, &thread())),)).build())
    }

    /// A TLS connection to `addr` trusting `der`, encrypted in user space.
    async fn tls_connect(addr: SocketAddr, der: &[u8]) -> TlsStream<TcpStream, Client> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(der.to_vec()))
            .expect("the generated certificate is a root");
        let config = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        let io = Compio::default();
        let stream = io.connect(addr).await.expect("connect");
        TlsConnector::new(Arc::new(config))
            .connect(
                ServerName::try_from("localhost").expect("a DNS name"),
                stream,
                &io,
            )
            .await
            .expect("the client handshake")
    }

    /// A fresh HTTP/1.1 connection over TLS to `addr`.
    async fn connect(addr: SocketAddr, der: &[u8]) -> http_kit_h1::SendRequest {
        let stream = tls_connect(addr, der).await;
        let (send, connection) = http_kit_h1::handshake_stream(
            stream,
            Compio::default(),
            http_kit_h1::ClientConfig::default(),
        );
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        send
    }

    /// Every connection `modes` records moved its records to the kernel.
    fn assert_kernel(modes: &Modes, server: &str) {
        let modes = modes.borrow();
        assert!(
            !modes.is_empty() && modes.iter().all(|mode| *mode == Mode::Kernel),
            "the {server}'s connections settled on {modes:?}; is the kernel's `tls` module loaded?"
        );
    }

    /// A request body goes from the client's kernel TLS socket into the upstream's through pipes.
    #[test]
    fn an_upload_is_spliced_between_kernel_tls_connections() {
        let ((), log) = run_logged(async {
            let (origin, _release) = Origin::new();
            let (origin_addr, origin_modes, origin_der) = serve_tls(origin.clone());
            let (proxy, proxy_modes, proxy_der) = proxy_to(origin_addr, origin_der);
            let send = connect(proxy, &proxy_der).await;
            let response = send
                .send(request(Method::PUT, "/api/upload", Body::from(bulk())))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert!(origin.last().body == bulk(), "the upload arrived changed");
            assert_kernel(&origin_modes, "origin");
            assert_kernel(&proxy_modes, "proxy");
        });
        assert_spliced(&log, bulk().len());
    }

    /// A response body goes from the upstream's kernel TLS socket into the client's through pipes.
    #[test]
    fn a_download_is_spliced_between_kernel_tls_connections() {
        let ((), log) = run_logged(async {
            let (origin, _release) = Origin::new();
            let (origin_addr, origin_modes, origin_der) = serve_tls(origin);
            let (proxy, proxy_modes, proxy_der) = proxy_to(origin_addr, origin_der);
            let send = connect(proxy, &proxy_der).await;
            let response = send
                .send(request(Method::GET, "/api/download", Body::empty()))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let (data, _) = read_frames(response.into_body()).await;
            assert!(data == bulk(), "the download arrived changed");
            assert_kernel(&origin_modes, "origin");
            assert_kernel(&proxy_modes, "proxy");
        });
        assert_spliced(&log, bulk().len());
    }

    /// A WebSocket tunnel between two kernel TLS connections splices both directions.
    #[cfg(feature = "ws")]
    #[test]
    fn a_tunnel_is_spliced_between_kernel_tls_connections() {
        use async_tungstenite::{client_async, tungstenite::Message};
        use futures_util::StreamExt as _;
        use http_kit_io::poll::CopyingPoll;

        use super::websocket::{echo_origin, GREETINGS};

        let ((), log) = run_logged(async {
            let (origin_addr, origin_modes, origin_der) = serve_tls(echo_origin());
            let (proxy, proxy_modes, proxy_der) = proxy_to(origin_addr, origin_der);
            let stream = CopyingPoll::new(tls_connect(proxy, &proxy_der).await);
            let (mut client, response) = client_async("wss://localhost/api/echo", stream)
                .await
                .expect("the WebSocket handshake through the proxy");
            assert_eq!(response.status(), 101);
            for greeting in GREETINGS {
                client.send(Message::text(greeting)).await.expect("send");
                let echoed = client
                    .next()
                    .await
                    .expect("an echo")
                    .expect("a WebSocket frame");
                assert_eq!(echoed.into_text().expect("text").as_str(), greeting);
            }
            client.close(None).await.expect("close");
            assert_kernel(&origin_modes, "origin");
            assert_kernel(&proxy_modes, "proxy");
        });
        assert!(
            log.contains("splicing a proxied upgrade"),
            "the tunnel between kernel TLS connections was copied"
        );
    }
}
