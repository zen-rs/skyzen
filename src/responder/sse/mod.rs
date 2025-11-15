//! [Server-Sent event (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events) responder.
//!
//! SSE allows a server to send new data to client at any time, which is useful in message pushing,etc.
//! There're two style to use [`SSE`](crate::responder::Sse) responder: stream style and channel style.
//! # Stream style
//! Convert a stream generating `Result<Event>` to a SSE stream.
//! ```ignore
//! # use skyzen::responder::{Sse,sse::Event};
//! use futures_util::stream::once;
//! use std::convert::Infallible;
//! async fn handler() -> Sse{
//!     Sse::from_stream(once(async{Ok::<_,Infallible>(Event::data("Hello!"))}))
//! }
//! ```
//!
//! # Channel style
//! Create a SSE stream and a sender, send message to stream with the sender.
//! *Warning:* You must return SSE stream first before you send message by sender.
//! ```ignore
//! use skyzen::responder::{Sse,sse::Event};
//! async fn handler() -> Sse{
//!     let(sender,sse) = Sse::channel();
//!     sender.send_data("Hello!");
//!     sse
//! }
//! ```
mod channel;
pub use channel::{SendError, Sender};

use itoa::Buffer;

use http_kit::{
    header::{self, HeaderValue},
    utils::Stream,
    Body, Request, Response,
};
use pin_project_lite::pin_project;
use serde::Serialize;
use skyzen_core::Responder;
use std::{
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

fn has_newline(v: &[u8]) -> bool {
    v.iter().any(|x| *x == b'\n' || *x == b'\r')
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
    pub fn data(data: impl AsRef<str>) -> Self {
        let mut event = Self::empty();
        let data = data.as_ref();
        event.field("data", data);
        event
    }

    /// Create an SSE event with a data payload in json format.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization of the value to JSON fails.
    pub fn json(v: impl Serialize) -> serde_json::Result<Self> {
        let mut event = Self::empty();
        event.buffer.extend_from_slice(b"data:");
        serde_json::to_writer(&mut event.buffer, &v)?;
        Ok(event)
    }

    /// A comment for the stream,being ignored by most of client.
    pub fn comment(message: impl AsRef<str>) -> Self {
        let mut event = Self::empty();
        let message = message.as_ref();
        event.field("", message);
        // Prevent including event and id in comment
        event.has_event_field = true;
        event.has_id = true;
        event
    }

    /// Tell the client the stream's reconnection time.
    #[must_use]
    pub fn retry(duration: Duration) -> Self {
        let mut event = Self::empty();
        event.field("retry", Buffer::new().format(duration.as_millis()));
        // Prevent including event and id in comment.
        event.has_event_field = true;
        event.has_id = true;
        event
    }

    /// Set the id of this event.
    /// The id is useful in reconnection.See [The `Last-Event-ID` header](https://html.spec.whatwg.org/multipage/server-sent-events.html#the-last-event-id-header) for more information.
    ///
    /// # Panics
    ///
    /// Panics if the id has already been set.
    #[must_use]
    pub fn id(mut self, id: impl AsRef<str>) -> Self {
        assert!(!self.has_id, "Id has alreay been set");
        let id = id.as_ref();
        self.field("id", id);
        self
    }

    /// Set the event of this event.
    ///
    /// # Panics
    ///
    /// Panics if the event has already been set.
    #[must_use]
    pub fn event(mut self, event: impl AsRef<str>) -> Self {
        assert!(!self.has_event_field, "Event has already been set");
        let event = event.as_ref();
        self.field("event", event);
        self.has_event_field = true;
        self
    }

    // Warning: the value cannot include `\r` or `\n`
    fn field(&mut self, name: &str, value: &str) {
        assert!(
            !has_newline(value.as_bytes()),
            "SSE field value cannot include newline"
        );

        self.buffer.extend_from_slice(name.as_bytes());

        self.buffer.extend_from_slice(b":");

        let value = value.as_bytes();

        if value.starts_with(b" ") {
            self.buffer.push(b' ');
        }

        self.buffer.extend_from_slice(value);

        self.buffer.extend_from_slice(b"\n");
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
    E: Into<anyhow::Error>,
{
    type Item = Result<Vec<u8>, anyhow::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(ready!(self.project().stream.poll_next(cx)))
            .map(|result| result.map(|data| data.map(Event::finalize).map_err(Into::into)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

impl Sse {
    /// Create a paif of sender and SSE responder
    #[must_use]
    pub fn channel() -> (Sender, Self) {
        Sender::new()
    }

    /// Create a SSE responder with a stream.
    pub fn from_stream<S, E>(stream: S) -> Self
    where
        S: Send + Sync + Stream<Item = Result<Event, E>> + 'static,
        E: Send + Sync + Into<anyhow::Error> + 'static,
    {
        Self {
            stream: Body::from_stream(IntoStream::new(stream)),
        }
    }
}

impl Responder for Sse {
    fn respond_to(self, _request: &Request, response: &mut Response) -> http_kit::Result<()> {
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
