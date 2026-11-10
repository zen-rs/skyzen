//! An HTTP/1.1 server on a loopback port, for exercising a backend's real HTTP client.
//!
//! A backend that speaks a REST API is only tested end to end when its requests leave through its
//! own client and come back through it: [`LoopbackServer`] answers them with whatever the test
//! says, on Skyzen's own HTTP/1.1 server running on a compio runtime of its own thread — the
//! driver the native server runs.

use core::convert::Infallible;
use std::{cell::RefCell, net::SocketAddr, rc::Rc, thread};

use async_channel::Sender;
use executor_core::compio::Runtime;
use http_kit::{error::BoxHttpError, utils::Bytes, Body, Endpoint, Request, Response};
use http_kit_io::{compio::Compio, Io as _};
use skyzen_core::Server as _;
use skyzen_h1::H1;

/// A request as it reached a [`LoopbackServer`]: its head and its whole body.
pub type ReceivedRequest = http::Request<Bytes>;

/// An HTTP/1.1 server on `127.0.0.1`, answering every request with a function of it.
///
/// The server stops when this handle is dropped.
#[derive(Debug)]
pub struct LoopbackServer {
    addr: SocketAddr,
    /// Closed on drop, which is the server's shutdown signal.
    _stop: Sender<Infallible>,
}

impl LoopbackServer {
    /// Serve on an ephemeral loopback port, answering each request — its body read in full — with
    /// `answer`.
    ///
    /// # Panics
    ///
    /// If the port cannot be bound, the server thread cannot be spawned, or its compio runtime
    /// cannot be built.
    pub fn start<F>(answer: F) -> Self
    where
        F: FnMut(ReceivedRequest) -> Response + Send + 'static,
    {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback test listener");
        let addr = listener
            .local_addr()
            .expect("the loopback listener has an address");
        let (stop, stopped) = async_channel::bounded::<Infallible>(1);

        thread::Builder::new()
            .name("skyzen-test-loopback".to_owned())
            .spawn(move || {
                let runtime = Rc::new(Runtime::new().expect("a compio runtime for the server"));
                let executor = Rc::clone(&runtime);
                runtime.block_on(async move {
                    let listener = Compio
                        .tcp_listener(listener)
                        .expect("register the listener with compio");
                    let endpoint = Answer(Rc::new(RefCell::new(answer)));
                    let shutdown = async move {
                        // Nothing is ever sent: the channel closes when the handle is dropped.
                        let _ = stopped.recv().await;
                    };
                    H1::default()
                        .serve(Compio, executor, listener, endpoint, shutdown)
                        .await;
                });
            })
            .expect("spawn the loopback server thread");

        Self { addr, _stop: stop }
    }

    /// The address the server listens on.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// The endpoint that hands each request to the test's function.
struct Answer<F>(Rc<RefCell<F>>);

impl<F> Clone for Answer<F> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl<F> Endpoint for Answer<F>
where
    F: FnMut(ReceivedRequest) -> Response + 'static,
{
    type Error = BoxHttpError;

    async fn respond(&mut self, request: &mut Request) -> Result<Response, Self::Error> {
        let (parts, body) = core::mem::replace(request, Request::new(Body::empty())).into_parts();
        let body = body
            .into_bytes()
            .await
            .expect("the loopback server reads the whole request body");
        let received = ReceivedRequest::from_parts(parts, body);
        Ok((self.0.borrow_mut())(received))
    }
}
