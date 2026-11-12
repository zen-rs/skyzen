//! Serving threads fed by one acceptor.
//!
//! Windows completion ports bind a socket to the one port it is first used on, so a listening
//! socket cannot be shared by the per-thread compio runtimes, and `SO_REUSEPORT` does not exist
//! there to give each its own. One thread therefore accepts on a socket compio never sees (the
//! readiness-based `async-io` reactor, which does not use completion ports) and deals the accepted
//! connections out round-robin; each serving thread adopts the ones it is dealt into its own
//! runtime. Windows is the only target that selects this path; the pieces here are portable, so
//! they are built and tested everywhere.
#![cfg_attr(
    not(any(windows, test)),
    expect(dead_code, reason = "only Windows selects the handoff path")
)]

use core::convert::Infallible;
use std::{io, net::SocketAddr};

use async_channel::{unbounded, Receiver, Sender};
use async_io::Async;
use futures_util::{future::Either, pin_mut};
use http_kit_io::{
    compio::{Compio, Ring, TcpStream},
    Listener,
};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::error;

use super::cores::BACKLOG;

/// A connection the acceptor has taken, with its peer.
type Accepted = (std::net::TcpStream, SocketAddr);

/// What a serving thread listens on instead of a socket of its own: the connections dealt to it.
#[derive(Debug)]
pub(super) struct Incoming {
    receiver: Receiver<Accepted>,
    local: SocketAddr,
}

impl Incoming {
    /// The dealt connections as the serving thread's listener, each adopted by `io`, the
    /// thread's driver for its ring, on that thread's runtime.
    pub(super) const fn adopted_by(self, io: Compio<Ring>) -> Dealt {
        Dealt { incoming: self, io }
    }

    /// The address the acceptor listens on.
    pub(super) const fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

/// [`Incoming`] on the serving thread, adopting each connection into its runtime.
#[derive(Debug)]
pub(super) struct Dealt {
    incoming: Incoming,
    io: Compio<Ring>,
}

impl Listener for Dealt {
    type Stream = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        match self.incoming.receiver.recv().await {
            Ok((stream, peer)) => {
                let stream = compio::net::TcpStream::from_std(stream)?;
                Ok((self.io.tcp_stream(stream), peer))
            }
            // The acceptor has stopped, which is what a closed listener means. The servers retry
            // a failed accept at once, so reporting it would spin until their shutdown signal,
            // which closes at the same moment, is polled; waiting for it is the same outcome.
            Err(_) => core::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.incoming.local)
    }
}

/// The accepting side: one listening socket and the channel to each serving thread.
#[derive(Debug)]
pub(super) struct Acceptor {
    listener: Async<std::net::TcpListener>,
    senders: Vec<Sender<Accepted>>,
}

impl Acceptor {
    /// Accept and deal connections out in turn until `stop` closes, then close every channel.
    ///
    /// Blocks the calling thread, which belongs to the acceptor.
    pub(super) fn run(self, stop: &Receiver<Infallible>) {
        let Self { listener, senders } = self;
        async_io::block_on(async {
            let mut next = 0;
            loop {
                let accepting = listener.accept();
                let stopped = stop.recv();
                pin_mut!(accepting, stopped);
                let (stream, peer) = match futures_util::future::select(accepting, stopped).await {
                    Either::Left((Ok((stream, peer)), _)) => (stream, peer),
                    Either::Left((Err(error), _)) => {
                        error!("accept failed: {error}");
                        continue;
                    }
                    Either::Right(_) => break,
                };
                let stream = match stream.into_inner() {
                    Ok(stream) => stream,
                    Err(error) => {
                        error!("cannot release an accepted connection: {error}");
                        continue;
                    }
                };
                let mut dealt = (stream, peer);
                // A thread that has gone leaves its connection to the next one.
                for _ in 0..senders.len() {
                    let sender = &senders[next];
                    next = (next + 1) % senders.len();
                    match sender.send(dealt).await {
                        Ok(()) => break,
                        Err(returned) => dealt = returned.into_inner(),
                    }
                }
            }
        });
        // `senders` drops here, closing every channel.
    }
}

/// Bind `addr` once and give each of `threads` serving threads an [`Incoming`], with the
/// [`Acceptor`] that feeds them.
///
/// # Errors
///
/// The address cannot be bound, or the reactor cannot watch the socket.
pub(super) fn bind(addr: SocketAddr, threads: usize) -> io::Result<(Vec<Incoming>, Acceptor)> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.bind(&addr.into())?;
    socket.listen(BACKLOG)?;
    let listener: std::net::TcpListener = socket.into();
    // Port 0 asked the system for a port.
    let local = listener.local_addr()?;
    let listener = Async::new(listener)?;
    let (senders, incoming) = (0..threads)
        .map(|_| {
            let (sender, receiver) = unbounded();
            (sender, Incoming { receiver, local })
        })
        .unzip();
    Ok((incoming, Acceptor { listener, senders }))
}
