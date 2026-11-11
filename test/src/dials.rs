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

use http_kit_io::{compio::Compio, time::Timer, Datagram, Dial};

/// compio's `http-kit-io` driver, reporting the thread every connection is dialed on.
///
/// It offers what a client dials with — TCP and Unix-domain connections, datagram sockets for
/// QUIC, and a timer — and everything but the report is [`Compio`]'s own, so it runs wherever
/// compio does: on a thread running a compio runtime.
#[derive(Debug, Clone)]
pub struct DialRecorder {
    io: Compio,
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
        (
            Self {
                io: Compio::default(),
                report,
            },
            Dials(dials),
        )
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
        self.io.sleep_until(deadline)
    }
}

impl Dial for DialRecorder {
    type Connection = <Compio as Dial>::Connection;

    fn connect(&self, addr: SocketAddr) -> impl Future<Output = io::Result<Self::Connection>> {
        self.record();
        self.io.connect(addr)
    }
}

#[cfg(unix)]
impl http_kit_io::DialUnix for DialRecorder {
    type UnixConnection = <Compio as http_kit_io::DialUnix>::UnixConnection;

    fn connect_unix(
        &self,
        path: &std::path::Path,
    ) -> impl Future<Output = io::Result<Self::UnixConnection>> {
        self.record();
        self.io.connect_unix(path)
    }
}

impl Datagram for DialRecorder {
    type UdpSocket = <Compio as Datagram>::UdpSocket;

    fn bind_udp(&self, addr: SocketAddr) -> impl Future<Output = io::Result<Self::UdpSocket>> {
        self.io.bind_udp(addr)
    }
}
