//! `#[skyzen::main]` end to end: the `native` example, run as a real process, serves requests on
//! the thread-per-core runtime and stops cleanly on `SIGINT`.

#![cfg(unix)]

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// The `native` example, built with the profile this test is built in; cargo reuses the build
/// when nothing changed.
fn example() -> PathBuf {
    escargot::CargoBuild::new()
        .example("native")
        .manifest_path(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .current_release()
        .run()
        .expect("build the native example")
        .path()
        .to_path_buf()
}

/// The running example, killed if a test fails before it is stopped.
struct Server(Child);

impl Server {
    fn start(args: &[&str]) -> Self {
        Self(
            Command::new(example())
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start the example"),
        )
    }

    /// Send `SIGINT` and return whether the process exited successfully.
    fn interrupt(mut self) -> bool {
        let status = Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -INT failed");
        self.0.wait().expect("wait for the example").success()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Already exited after `interrupt`; otherwise this is a failing test cleaning up.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Retry `connect` until the server accepts, for at most 30 seconds.
fn connect_when_ready<S>(mut connect: impl FnMut() -> std::io::Result<S>) -> S {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match connect() {
            Ok(stream) => return stream,
            Err(error) if Instant::now() >= deadline => panic!("the server never came up: {error}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn exchange(mut stream: impl std::io::Read + std::io::Write) -> String {
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .expect("write the request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    response
}

/// What follows the response head, framed however the server chose to frame it.
fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

#[test]
fn the_example_serves_over_tcp_and_stops_on_sigint() {
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("local addr").port()
    };
    let server = Server::start(&["--port", &port.to_string()]);

    let response = exchange(connect_when_ready(|| {
        std::net::TcpStream::connect(("127.0.0.1", port))
    }));

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(body(&response).contains("OK"), "{response}");
    assert!(server.interrupt(), "SIGINT is a graceful shutdown");
}

#[test]
fn the_example_serves_a_unix_socket_and_removes_it_on_sigint() {
    let path = std::env::temp_dir().join(format!("skyzen-native-{}.sock", std::process::id()));
    let server = Server::start(&["--listen", &format!("unix:{}", path.display())]);

    let response = exchange(connect_when_ready(|| {
        std::os::unix::net::UnixStream::connect(&path)
    }));

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(body(&response).contains("OK"), "{response}");
    assert!(server.interrupt(), "SIGINT is a graceful shutdown");
    assert!(!path.exists(), "the socket file is removed on shutdown");
}

#[test]
fn the_example_serves_a_unix_socket_with_hyper_too() {
    let path =
        std::env::temp_dir().join(format!("skyzen-native-hyper-{}.sock", std::process::id()));
    let server = Server::start(&[
        "--server",
        "hyper",
        "--listen",
        &format!("unix:{}", path.display()),
    ]);

    let response = exchange(connect_when_ready(|| {
        std::os::unix::net::UnixStream::connect(&path)
    }));

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(body(&response).contains("OK"), "{response}");
    assert!(server.interrupt(), "SIGINT is a graceful shutdown");
    assert!(!path.exists(), "the socket file is removed on shutdown");
}

/// What the example printed and how it ended, for a start that is expected to fail.
fn refused(args: &[&str]) -> (bool, String) {
    let output = Command::new(example())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run the example");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), log)
}

/// `--http3` is a promise: a server that cannot keep it exits non-zero and names the reason,
/// whichever it is (no `http3` feature in this build, no TLS, a unix socket).
#[test]
fn the_example_refuses_to_start_when_http3_is_requested_and_cannot_be_served() {
    for args in [
        &["--http3"][..],
        &["--http3", "--listen", "unix:/tmp/skyzen-http3-refused.sock"][..],
    ] {
        let (success, log) = refused(args);
        assert!(!success, "{args:?} started: {log}");
        assert!(log.contains("HTTP/3 is requested"), "{args:?}: {log}");
    }
}
