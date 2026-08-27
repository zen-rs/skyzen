//! [Server-Sent event (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events) responder.
//!
//! SSE allows a server to send new data to client at any time, which is useful in message pushing,etc.
//! There're two style to use [`SSE`](crate::responder::Sse) responder: stream style and channel style.
//! # Stream style
//! Convert a stream generating `Result<Event>` to a SSE stream.
//! ```
//! # use skyzen::responder::{Sse,sse::Event};
//! use futures_util::stream::once;
//! use std::convert::Infallible;
//! async fn handler() -> Sse{
//!     Sse::from_stream(once(async{Ok::<_,Infallible>(Event::data("Hello!"))}))
//! }
//! ```
//!
//! # Channel style
//! Create a SSE stream and a sender, then push messages from another task while the handler
//! returns the [`Sse`](crate::responder::Sse) responder so the stream starts flowing. The channel
//! is bounded, so `send` awaits when the buffer is full.
//! ```
//! use skyzen::responder::{Sse,sse::Event};
//! async fn handler() -> Sse{
//!     let(sender,sse) = Sse::channel();
//!     // Drive `sender` from a spawned task using your runtime, e.g.
//!     // tokio::spawn(async move { sender.send_data("Hello!").await.ok(); });
//!     let _ = sender;
//!     sse
//! }
//! ```
mod channel;
pub use channel::{SendError, Sender};

use futures_util::TryStreamExt;
use itoa::Buffer;

use http_kit::{
    header::{self, HeaderValue},
    utils::Stream,
    Body, BodyError, Request, Response,
};
use pin_project_lite::pin_project;
#[cfg(feature = "json")]
use serde::Serialize;
use skyzen_core::Responder;
use std::{
    convert::Infallible,
    marker::PhantomData,
    pin::Pin,
    task::{ready, Context, Poll},
    time::Duration,
};

/// A SSE event
#[derive(Debug)]
pub struct Event {
    buffer: Vec<u8>,
    has_id: bool,
    has_event_field: bool,
}

impl Event {
    const fn empty() -> Self {
        Self {
            buffer: Vec::new(),
            has_id: false,
            has_event_field: false,
        }
    }

    /// Create an SSE event with a data payload.
    ///
    /// Per the SSE specification a multi-line payload is encoded as one `data:` line per source
    /// line (split on `LF`, `CR`, or `CRLF`), so newlines in user data are preserved correctly
    /// rather than corrupting the stream framing.
    pub fn data(data: impl AsRef<str>) -> Self {
        let mut event = Self::empty();
        event.push_multiline_field("data", data.as_ref());
        event
    }

    /// Create an SSE event with a data payload in json format.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization of the value to JSON fails.
    #[cfg(feature = "json")]
    pub fn json(v: impl Serialize) -> serde_json::Result<Self> {
        let mut event = Self::empty();
        event.buffer.extend_from_slice(b"data:");
        serde_json::to_writer(&mut event.buffer, &v)?;
        event.buffer.push(b'\n');
        Ok(event)
    }

    /// A comment for the stream, ignored by most clients. Multi-line comments are encoded as one
    /// `:` line per source line.
    pub fn comment(message: impl AsRef<str>) -> Self {
        let mut event = Self::empty();
        event.push_multiline_field("", message.as_ref());
        // A comment cannot carry an id or event name.
        event.has_event_field = true;
        event.has_id = true;
        event
    }

    /// Tell the client the stream's reconnection time.
    #[must_use]
    pub fn retry(duration: Duration) -> Self {
        let mut event = Self::empty();
        event.push_field_line("retry", Buffer::new().format(duration.as_millis()));
        // A retry directive cannot carry an id or event name.
        event.has_event_field = true;
        event.has_id = true;
        event
    }

    /// Set the id of this event.
    /// The id is useful in reconnection.See [The `Last-Event-ID` header](https://html.spec.whatwg.org/multipage/server-sent-events.html#the-last-event-id-header) for more information.
    ///
    /// # Panics
    ///
    /// Panics if the id has already been set — a programmer error in handler code, not reachable
    /// from request data.
    #[must_use]
    pub fn id(mut self, id: impl AsRef<str>) -> Self {
        assert!(!self.has_id, "Id has already been set");
        self.push_field_line("id", id.as_ref());
        self.has_id = true;
        self
    }

    /// Set the event name of this event.
    ///
    /// # Panics
    ///
    /// Panics if the event name has already been set — a programmer error in handler code, not
    /// reachable from request data.
    #[must_use]
    pub fn event(mut self, event: impl AsRef<str>) -> Self {
        assert!(!self.has_event_field, "Event has already been set");
        self.push_field_line("event", event.as_ref());
        self.has_event_field = true;
        self
    }

    /// Append a single SSE field line (`name:value`).
    ///
    /// `CR`/`LF` are stripped because a single field value cannot span lines; multi-line content
    /// must go through [`Self::push_multiline_field`].
    fn push_field_line(&mut self, name: &str, value: &str) {
        self.buffer.extend_from_slice(name.as_bytes());
        self.buffer.push(b':');

        // The SSE parser strips exactly one space after the colon, so preserve a leading space.
        if value.as_bytes().first() == Some(&b' ') {
            self.buffer.push(b' ');
        }

        self.buffer.extend(
            value
                .bytes()
                .filter(|byte| *byte != b'\r' && *byte != b'\n'),
        );
        self.buffer.push(b'\n');
    }

    /// Append a possibly multi-line value as one `name:` line per source line, splitting on `LF`,
    /// `CR`, or `CRLF`.
    fn push_multiline_field(&mut self, name: &str, value: &str) {
        let mut rest = value;
        loop {
            let Some(idx) = rest.find(['\r', '\n']) else {
                self.push_field_line(name, rest);
                break;
            };
            self.push_field_line(name, &rest[..idx]);
            let after = &rest[idx..];
            rest = after.strip_prefix("\r\n").unwrap_or_else(|| &after[1..]);
        }
    }

    fn finalize(mut self) -> Vec<u8> {
        self.buffer.push(b'\n');
        self.buffer
    }
}

/// SSE responder
#[derive(Debug)]
pub struct Sse {
    stream: Body,
}

pin_project! {
    struct IntoStream<S,E>{
        #[pin]
        stream:S,
        _marker:PhantomData<E>
    }
}

impl<S, E> IntoStream<S, E> {
    pub const fn new(stream: S) -> Self {
        Self {
            stream,
            _marker: PhantomData,
        }
    }
}

impl<S, E> Stream for IntoStream<S, E>
where
    S: Stream<Item = Result<Event, E>>,
    E: core::error::Error + Send + Sync + 'static,
{
    type Item = Result<Vec<u8>, E>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(ready!(self.project().stream.poll_next(cx)))
            .map(|result| result.map(|data| data.map(Event::finalize)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

impl Sse {
    /// Create a pair of sender and SSE responder backed by a bounded channel with the
    /// [default capacity](channel::DEFAULT_CHANNEL_CAPACITY).
    #[must_use]
    pub fn channel() -> (Sender, Self) {
        Sender::new()
    }

    /// Create a sender/responder pair whose bounded channel buffers up to `capacity` events before
    /// [`Sender::send`] applies backpressure.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    #[must_use]
    pub fn channel_with_capacity(capacity: usize) -> (Sender, Self) {
        Sender::with_capacity(capacity)
    }

    /// Create a SSE responder with a stream.
    pub fn from_stream<S, E>(stream: S) -> Self
    where
        S: Send + Sync + Stream<Item = Result<Event, E>> + 'static,
        E: Send + Sync + core::error::Error + 'static,
    {
        Self {
            // Erase the stream's concrete error into the body error channel.
            stream: Body::from_stream(
                IntoStream::new(stream).map_err(|error| BodyError::Other(Box::new(error))),
            ),
        }
    }

    /// Send a comment every `interval` while the stream has nothing to say.
    ///
    /// Proxies and load balancers close a connection that has been idle for 30–60 seconds, which
    /// is exactly what a long-lived event stream looks like between events. A comment is ignored
    /// by every client but keeps the bytes flowing, so the connection survives.
    ///
    /// The countdown restarts whenever a real event goes out, so an active stream sends no
    /// comments at all.
    ///
    /// ```rust
    /// # #[cfg(not(target_arch = "wasm32"))] {
    /// use std::time::Duration;
    /// use skyzen::responder::Sse;
    ///
    /// async fn events() -> Sse {
    ///     let (sender, sse) = Sse::channel();
    ///     let _ = sender;
    ///     sse.keep_alive(Duration::from_secs(15))
    /// }
    /// # }
    /// ```
    ///
    /// Native targets only: a heartbeat needs a platform timer, and WebAssembly isolates cap a
    /// response's lifetime themselves, so there is no idle connection to keep alive there.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn keep_alive(self, interval: Duration) -> Self {
        Self {
            stream: Body::from_stream(KeepAlive::new(self.stream, interval)),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pin_project! {
    /// Merges a periodic comment into a stream that has gone quiet.
    struct KeepAlive {
        #[pin]
        inner: Body,
        // `async_io::Timer` is `Unpin`, so it is driven through `Pin::new` rather than projected —
        // which is also what lets the interval be restarted when real data flows.
        timer: async_io::Timer,
        interval: Duration,
        finished: bool,
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl KeepAlive {
    fn new(inner: Body, interval: Duration) -> Self {
        Self {
            inner,
            timer: async_io::Timer::interval(interval),
            interval,
            finished: false,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Stream for KeepAlive {
    type Item = <Body as Stream>::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        if *this.finished {
            return Poll::Ready(None);
        }

        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(item)) => {
                // Real traffic is its own keep-alive: push the next heartbeat out a full interval.
                this.timer.set_interval(*this.interval);
                return Poll::Ready(Some(item));
            }
            Poll::Ready(None) => {
                *this.finished = true;
                return Poll::Ready(None);
            }
            Poll::Pending => {}
        }

        match Pin::new(&mut *this.timer).poll_next(cx) {
            Poll::Ready(_) => Poll::Ready(Some(Ok(Event::comment("").finalize().into()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Responder for Sse {
    type Error = Infallible;
    fn respond_to(self, _request: &Request, response: &mut Response) -> Result<(), Self::Error> {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        *response.body_mut() = self.stream;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, time::Duration};

    use futures_util::stream::once;
    use http_kit::HttpError;
    use serde::Serialize;
    use skyzen_core::Responder;

    use super::{Event, Sse};
    use crate::{header, Body, Request, Response};

    #[derive(Debug, Serialize)]
    struct Payload {
        name: &'static str,
    }

    #[test]
    fn formats_data_event_with_id_and_event_name() {
        let bytes = Event::data("hello").id("evt-1").event("message").finalize();

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "data:hello\nid:evt-1\nevent:message\n\n"
        );
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_event_serializes_payload() {
        let bytes = Event::json(Payload { name: "skyzen" }).unwrap().finalize();

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "data:{\"name\":\"skyzen\"}\n\n"
        );
    }

    #[test]
    fn comment_and_retry_events_have_expected_wire_format() {
        assert_eq!(
            String::from_utf8(Event::comment("keepalive").finalize()).unwrap(),
            ":keepalive\n\n"
        );
        assert_eq!(
            String::from_utf8(Event::retry(Duration::from_millis(1500)).finalize()).unwrap(),
            "retry:1500\n\n"
        );
    }

    #[test]
    fn data_event_splits_multiple_lines() {
        let bytes = Event::data("hello\nworld").finalize();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "data:hello\ndata:world\n\n"
        );
    }

    #[test]
    fn data_event_handles_crlf_and_cr_terminators() {
        let bytes = Event::data("a\r\nb\rc").finalize();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "data:a\ndata:b\ndata:c\n\n"
        );
    }

    #[test]
    fn id_strips_stray_newlines_to_protect_framing() {
        let bytes = Event::data("x").id("a\nb").finalize();
        assert_eq!(String::from_utf8(bytes).unwrap(), "data:x\nid:ab\n\n");
    }

    #[tokio::test]
    async fn from_stream_sets_headers_and_serializes_event_stream() {
        let request = Request::new(Body::empty());
        let mut response = Response::new(Body::empty());
        let sse = Sse::from_stream(once(async { Ok::<_, Infallible>(Event::data("hello")) }));

        sse.respond_to(&request, &mut response).unwrap();

        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "data:hello\n\n");
    }

    #[tokio::test]
    async fn channel_sender_delivers_events_and_closes_cleanly() {
        let (sender, sse) = Sse::channel();
        let request = Request::new(Body::empty());
        let mut response = Response::new(Body::empty());

        sse.respond_to(&request, &mut response).unwrap();
        sender.send_data("hello").await.unwrap();
        drop(sender);

        let body = response.into_body().into_string().await.unwrap();
        assert_eq!(body, "data:hello\n\n");
    }

    #[tokio::test]
    async fn sender_reports_error_after_stream_is_dropped() {
        let (sender, sse) = Sse::channel();
        drop(sse);

        let error = sender.send_data("hello").await.unwrap_err();
        assert_eq!(error.status(), crate::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn keep_alive_sends_a_comment_while_the_stream_is_idle() {
        use futures_util::StreamExt;

        let (sender, sse) = Sse::channel();
        let sse = sse.keep_alive(Duration::from_millis(20));

        let mut response = Response::new(Body::empty());
        sse.respond_to(&Request::new(Body::empty()), &mut response)
            .unwrap();

        // Nothing has been sent, so the only thing that can arrive is the heartbeat.
        let mut body = response.into_body();
        let chunk = body
            .next()
            .await
            .expect("the heartbeat keeps the stream open")
            .expect("a comment is not an error");
        assert_eq!(chunk.as_ref(), b":\n\n");

        // A real event still comes through the same stream.
        sender.send_data("hello").await.unwrap();
        let mut seen = Vec::new();
        while let Some(Ok(chunk)) = body.next().await {
            if chunk.as_ref() == b"data:hello\n\n" {
                seen.push(chunk);
                break;
            }
        }
        assert_eq!(seen.len(), 1, "the real event reaches the client");
    }
}
