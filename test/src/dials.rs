//! An `http-kit-io` driver that reports which thread dials each connection.
//!
//! A client whose connection pool is meant to live on the thread that uses it is only shown to do
//! so by watching where its sockets are opened: [`DialRecorder`] is compio's driver with every
//! dial reported, by thread, to the [`Dials`] it was created with.

use std::{
    future::Future,
    io,
    net::SocketAddr,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, ThreadId},
    time::Instant,
};

use http_kit_io::{compio::Compio, time::Timer, Io};

/// compio's `http-kit-io` driver, reporting the thread every connection is dialed on.
///
/// Everything but the report is [`Compio`]'s own, so it runs wherever compio does: on a thread
/// running a compio runtime.
#[derive(Debug, Clone)]
pub struct DialRecorder {
    report: Sender<ThreadId>,
}

/// Where a [`DialRecorder`] reports: the thread of each dial, in the order they began.
#[derive(Debug)]
pub struct Dials(Receiver<ThreadId>);

impl DialRecorder {
    /// A recorder and the [`Dials`] it reports to.
    #[must_use]
    pub fn new() -> (Self, Dials) {
        let (report, dials) = mpsc::channel();
        (Self { report }, Dials(dials))
    }

    fn record(&self) {
        // The test that holds `Dials` may already have stopped reading; a dial it no longer
        // watches has no one to be reported to.
        let _ = self.report.send(thread::current().id());
    }
}

impl Dials {
    /// The threads every dial reported so far began on.
    #[must_use]
    pub fn taken(&self) -> Vec<ThreadId> {
        self.0.try_iter().collect()
    }
}

impl Timer for DialRecorder {
    fn sleep_until(&self, deadline: Instant) -> impl Future<Output = ()> {
        Compio.sleep_until(deadline)
    }
}

impl Io for DialRecorder {
    type TcpStream = <Compio as Io>::TcpStream;
    type TcpListener = <Compio as Io>::TcpListener;
    #[cfg(unix)]
    type UnixStream = <Compio as Io>::UnixStream;
    #[cfg(unix)]
    type UnixListener = <Compio as Io>::UnixListener;
    type UdpSocket = <Compio as Io>::UdpSocket;

    fn connect(&self, addr: SocketAddr) -> impl Future<Output = io::Result<Self::TcpStream>> {
        self.record();
        Compio.connect(addr)
    }

    #[cfg(unix)]
    fn connect_unix(
        &self,
        path: &std::path::Path,
    ) -> impl Future<Output = io::Result<Self::UnixStream>> {
        self.record();
        Compio.connect_unix(path)
    }

    fn tcp_listener(&self, listener: std::net::TcpListener) -> io::Result<Self::TcpListener> {
        Compio.tcp_listener(listener)
    }

    #[cfg(unix)]
    fn unix_listener(
        &self,
        listener: std::os::unix::net::UnixListener,
    ) -> io::Result<Self::UnixListener> {
        Compio.unix_listener(listener)
    }

    fn udp_socket(&self, socket: std::net::UdpSocket) -> io::Result<Self::UdpSocket> {
        Compio.udp_socket(socket)
    }
}
