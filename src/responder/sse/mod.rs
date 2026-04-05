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
//! Create a SSE stream and a sender, send message to stream with the sender.
//! *Warning:* You must return SSE stream first before you send message by sender.
//! ```
//! use skyzen::responder::{Sse,sse::Event};
//! async fn handler() -> Sse{
//!     let(sender,sse) = Sse::channel();
//!     sender.send_data("Hello!");
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
    #[cfg(feature = "json")]
    pub fn json(v: impl Serialize) -> serde_json::Result<Self> {
        let mut event = Self::empty();
        event.buffer.extend_from_slice(b"data:");
        serde_json::to_writer(&mut event.buffer, &v)?;
        event.buffer.push(b'\n');
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
        assert!(!self.has_id, "Id has already been set");
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
    /// Create a paif of sender and SSE responder
    #[must_use]
    pub fn channel() -> (Sender, Self) {
        Sender::new()
    }

    /// Create a SSE responder with a stream.
    pub fn from_stream<S, E>(stream: S) -> Self
    where
        S: Send + Sync + Stream<Item = Result<Event, E>> + 'static,
        E: Send + Sync + core::error::Error + 'static,
    {
        Self {
            stream: Body::from_stream(IntoStream::new(stream).map_err(|error| {
                BodyError::Other(Box::new(error)) // TODO: improve error handling, currently we just box the error
            })),
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
    #[should_panic(expected = "SSE field value cannot include newline")]
    fn event_fields_reject_newlines() {
        let _ = Event::data("hello\nworld");
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
}
