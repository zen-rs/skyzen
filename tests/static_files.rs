//! `StaticDir` over Skyzen's own HTTP/1.1 server on compio, the way `#[skyzen::main]` serves it:
//! a file above the inline limit leaves as a file region, which the server hands to the socket's
//! `SendFile` (`splice` on Linux, `sendfile` on Apple platforms) instead of reading it.
//!
//! The observable is the server's own record: `http-kit-h1` emits a `sending a file region` trace
//! event exactly where it calls `send_file_all`, and nowhere else.

use std::{
    io::{Read as _, Write as _},
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, Mutex},
};

use executor_core::compio::Runtime;
use http_kit_io::{compio::Compio, Io as _, Listener as _};
use skyzen::{
    routing::{build, Route},
    static_files::{StaticCacheSettings, StaticDir},
    Server as _,
};
use skyzen_h1::H1;

/// Chinese, Japanese and Korean, so the bytes that cross the socket are not ASCII.
const CJK: &str = "你好，世界。こんにちは、世界。안녕하세요, 세계.";

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// Serve `dir` at `/s` on a compio thread of its own, every event that thread records going to
/// `log`, and return the bound address.
fn start(dir: std::path::PathBuf, log: Captured) -> SocketAddr {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(log)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let runtime = Rc::new(Runtime::new().expect("compio runtime"));
            runtime.block_on(async {
                let listener = Compio
                    .bind_tcp("127.0.0.1:0".parse().unwrap())
                    .expect("bind loopback");
                addr_tx
                    .send(listener.local_addr().expect("local addr"))
                    .expect("the test is waiting for the address");
                // Inline limit zero: every file is cached as an open descriptor and sent as a
                // region.
                let router = build(Route::new((
                    StaticDir::new("/s", dir).cache(StaticCacheSettings::new(16, 0)),
                )))
                .expect("the router builds");
                H1::default()
                    .serve(
                        Compio,
                        Rc::clone(&runtime),
                        listener,
                        router,
                        std::future::pending(),
                    )
                    .await;
            });
        });
    });
    addr_rx.recv().expect("the server thread binds")
}

/// One request on a fresh connection; the head, and the body that followed it.
fn get(addr: SocketAddr, path: &str, extra: &str) -> (String, Vec<u8>) {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra}\r\n"
    )
    .expect("write the request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read the response");
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a complete head");
    let head = String::from_utf8_lossy(&response[..end]).into_owned();
    (head, response[end + 4..].to_vec())
}

#[test]
fn file_regions_and_their_ranges_go_through_send_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let payload: Vec<u8> = (0..20_000)
        .flat_map(|line| format!("{line:06} {CJK}\n").into_bytes())
        .collect();
    // A Japanese name, percent-encoded on the request line below.
    std::fs::write(dir.path().join("データ.txt"), &payload).expect("write the payload");
    let encoded = "%E3%83%87%E3%83%BC%E3%82%BF.txt";

    let log = Captured::default();
    let addr = start(dir.path().to_path_buf(), log.clone());

    let (head, body) = get(addr, &format!("/s/{encoded}"), "");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(body == payload, "the region arrived changed");

    let (head, body) = get(
        addr,
        &format!("/s/{encoded}"),
        "Range: bytes=1000-50999\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert!(
        body == payload[1000..51000],
        "the ranged region arrived changed"
    );

    let log = String::from_utf8(log.0.lock().expect("log lock").clone()).expect("UTF-8 log");
    let regions: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("sending a file region"))
        .collect();
    assert_eq!(regions.len(), 2, "both responses went to send_file:\n{log}");
    assert!(
        regions[1].contains("offset=1000") && regions[1].contains("len=50000"),
        "the range was sent as a region of the file, not read out: {}",
        regions[1]
    );
}
