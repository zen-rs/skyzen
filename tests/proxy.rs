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
use http_kit_io::{compio::Compio, Io};
use ipnet::IpNet;
use skyzen::{
    extract::TrustedProxies,
    header::{self, HeaderMap, HeaderValue},
    proxy::{Proxy, Upstream},
    routing::{CreateRouteNode, Route},
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
    let listener = Compio.tcp_listener(listener).expect("register listener");
    compio::runtime::spawn(async move {
        H1::default()
            .serve_listener(
                Compio,
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

/// The scenario's three parties.
struct Stage {
    origin: Origin,
    proxy: SocketAddr,
    release: async_channel::Sender<()>,
}

/// An origin, and a proxy mounted at `/api` forwarding to it under `/v1`, with `configure` applied
/// to the proxy's route.
fn stage(configure: impl FnOnce(Route) -> Route) -> Stage {
    let (release, released) = async_channel::unbounded();
    let origin = Origin {
        seen: Rc::default(),
        release: released,
        produced: Arc::default(),
    };
    let origin_addr = serve(origin.clone());
    let target = format!("http://{origin_addr}/v1")
        .parse()
        .expect("upstream URI");
    let upstream = Upstream::new(&target).expect("upstream");
    let router = configure(Route::new(("/api".mount(Proxy::new(&upstream)),))).build();
    Stage {
        origin,
        proxy: serve(router),
        release,
    }
}

/// A fresh HTTP/1.1 connection to `addr`.
async fn connect(addr: SocketAddr) -> http_kit_h1::SendRequest {
    let stream = Compio.connect(addr).await.expect("connect");
    let (send, connection) =
        http_kit_h1::handshake_stream(stream, Compio, http_kit_h1::ClientConfig::default());
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
        let proxy = serve(Route::new(("/".mount(Proxy::new(&upstream)),)).build());
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
        let proxy = serve(Route::new(("/".mount(Proxy::new(&upstream)),)).build());
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
    use http_kit_io::{compio::Compio, poll::CopyingPoll, IntoPoll as _, Io};
    use skyzen::{
        proxy::{Proxy, Upstream},
        routing::{CreateRouteNode, Route},
        websocket::WebSocketMessage,
    };

    use super::{run, serve};

    /// The text messages: Chinese, Japanese and Korean.
    const GREETINGS: [&str; 3] = ["你好世界", "こんにちは", "안녕하세요"];
    /// The message that makes the origin close the connection itself.
    const FAREWELL: &str = "再见 / さようなら / 안녕히 가세요";

    type Client = WebSocketStream<CopyingPoll<<Compio as Io>::TcpStream>>;

    /// A WebSocket echo origin, and a proxy mounted at `/api` forwarding to it under `/v1`;
    /// returns the client's connection through the proxy, and the `Via` of its `101`.
    async fn connect() -> (Client, String) {
        let origin = serve(
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
            .build(),
        );
        let target = format!("http://{origin}/v1").parse().expect("upstream URI");
        let upstream = Upstream::new(&target).expect("upstream");
        let proxy = serve(Route::new(("/api".mount(Proxy::new(&upstream)),)).build());

        let stream = Compio.connect(proxy).await.expect("connect");
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

    #[test]
    fn text_and_binary_messages_round_trip_through_the_tunnel() {
        run(async {
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
