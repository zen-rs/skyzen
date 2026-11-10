//! Unix domain sockets on the compio driver.
//!
//! `http-kit-io` has a listener only for TCP, so a filesystem socket cannot reach
//! [`skyzen_h1::H1`] through `Io::TcpListener`. What it can reach is [`Accept`], the one
//! abstraction the connection loop takes its connections from: this module implements it for
//! compio's own unix socket, with the streams' `Read`/`Write` over the same owned-buffer
//! operations the TCP stream uses. The connection loop, its limits, its keep-alive and its drain
//! are `H1`'s, not a second copy of them.
//!
//! The one difference a connection can observe is that nothing is sent with `sendfile`: a
//! file-region body is read in chunks and written, which is also what `http-kit-io` does on a
//! platform that has no `sendfile`. A unix socket here is the hop to a fronting proxy on the same
//! machine, so the response bytes are the same and only the kernel-side shortcut is absent.

use std::{
    borrow::Borrow, fs::File, future::Future, io, net::SocketAddr, os::unix::fs::FileExt, rc::Rc,
};

use compio::{
    buf::{BufResult, IoBuf, IoBufMut, IoVectoredBuf},
    io::{AsyncRead as _, AsyncWrite as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
};
use http_kit_io::{Read, SendFile, Split, Write};
use skyzen_h1::Accept;

/// The most one file-region round reads, the default capacity of a Linux pipe, which bounds
/// `http-kit-io`'s own `splice` rounds the same way.
const FILE_CHUNK: usize = 64 * 1024;

/// A listening unix socket, registered with the current thread's runtime.
#[derive(Debug)]
pub(super) struct Listener(UnixListener);

impl Listener {
    /// Adopt a bound, listening std socket.
    pub(super) fn from_std(listener: std::os::unix::net::UnixListener) -> io::Result<Self> {
        UnixListener::from_std(listener).map(Self)
    }
}

impl Accept for Listener {
    type Stream = Stream;

    async fn accept(&self) -> io::Result<(Stream, Option<SocketAddr>)> {
        // A unix peer has no `SocketAddr`; requests on this connection carry no `PeerAddr`.
        let (stream, _) = self.0.accept().await?;
        Ok((Stream(stream), None))
    }
}

/// An accepted unix connection.
#[derive(Debug)]
pub(super) struct Stream(UnixStream);

impl Split for Stream {
    type ReadHalf = ReadHalf;
    type WriteHalf = WriteHalf;

    fn split(self) -> (ReadHalf, WriteHalf) {
        let shared = Rc::new(self.0);
        (ReadHalf(Rc::clone(&shared)), WriteHalf(shared))
    }
}

/// The reading half of a [`Stream`].
#[derive(Debug)]
pub(super) struct ReadHalf(Rc<UnixStream>);

/// The writing half of a [`Stream`].
#[derive(Debug)]
pub(super) struct WriteHalf(Rc<UnixStream>);

impl Read for ReadHalf {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        // The contract is a length of exactly what was read, from the start of the buffer.
        buf.clear();
        let mut stream = &*self.0;
        stream.read(buf).await
    }
}

impl Write for WriteHalf {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let mut stream = &*self.0;
        stream.write(buf).await
    }

    async fn write_vectored<B: IoVectoredBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let mut stream = &*self.0;
        stream.write_vectored(buf).await
    }

    fn flush(&mut self) -> impl Future<Output = io::Result<()>> {
        // A socket buffers nothing of its own.
        std::future::ready(Ok(()))
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        let mut stream = &*self.0;
        stream.shutdown().await
    }
}

impl SendFile for WriteHalf {
    async fn send_file<F: Borrow<File> + 'static>(
        &mut self,
        file: F,
        offset: u64,
        len: usize,
    ) -> BufResult<usize, F> {
        if len == 0 {
            return BufResult(Ok(0), file);
        }
        let mut chunk = vec![0; len.min(FILE_CHUNK)];
        // A positional read, synchronous like the page-cache miss `sendfile` would take.
        let read = FileExt::read_at(file.borrow(), &mut chunk, offset);
        let read = match read {
            Ok(read) => read,
            Err(error) => return BufResult(Err(error), file),
        };
        chunk.truncate(read);
        let mut stream = &*self.0;
        let BufResult(written, _) = stream.write_all(chunk).await;
        BufResult(written.map(|()| read), file)
    }
}
