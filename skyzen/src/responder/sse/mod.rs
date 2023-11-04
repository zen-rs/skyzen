//! [Server-Sent event (SSE)](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events) responder for levin

mod channel;
pub use channel::Sender;

use itoa::Buffer;

use std::{
    marker::PhantomData,
    pin::Pin,
    task::{ready, Context, Poll},
    time::Duration,
};

use futures_core::Stream;
use http_kit::{
    header::{self, HeaderValue},
    Body, Request, Response,
};
use pin_project_lite::pin_project;
use serde::Serialize;
use skyzen_core::Responder;

/// A SSE event
#[derive(Debug)]
pub struct Event {
    buffer: Vec<u8>,
    has_id: bool,
    has_event: bool,
}

fn has_newline(v: &[u8]) -> bool {
    v.iter().find(|x| **x == b'\n' || **x == b'\r').is_some()
}

impl Event {
    fn empty() -> Self {
        Self {
            buffer: Vec::new(),
            has_id: false,
            has_event: false,
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
    pub fn json(v: &impl Serialize) -> serde_json::Result<Self> {
        let mut event = Self::empty();
        event.buffer.extend_from_slice(b"data:");
        serde_json::to_writer(&mut event.buffer, v)?;
        Ok(event)
    }

    /// A comment for the stream,being ignored by most of client.
    pub fn comment(message: impl AsRef<str>) -> Self {
        let mut event = Self::empty();
        let message = message.as_ref();
        event.field("", message);
        // Prevent including event and id in comment
        event.has_event = true;
        event.has_id = true;
        event
    }

    /// Tell the client the stream's reconnection time.
    pub fn retry(duration: Duration) -> Self {
        let mut event = Self::empty();
        event.field("retry", Buffer::new().format(duration.as_millis()));
        // Prevent including event and id in comment.
        event.has_event = true;
        event.has_id = true;
        event
    }

    /// Set the id of this event.
    /// The id is useful in reconnection.See [The `Last-Event-ID` header](https://html.spec.whatwg.org/multipage/server-sent-events.html#the-last-event-id-header) for more information.
    pub fn id(&mut self, id: impl AsRef<str>) {
        assert!(!self.has_id, "Id has alreay been set");
        let id = id.as_ref();
        self.field("id", id)
    }

    /// Set the event of this event.
    pub fn event(&mut self, event: impl AsRef<str>) {
        assert!(!self.has_event, "Id has alreay been set");
        let event = event.as_ref();
        self.field("event", event)
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
    pub fn new(stream: S) -> Self {
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
        Poll::Ready(ready!(self.project().stream.poll_next(cx)).map(|result| {
            result
                .map(|data| data.finalize())
                .map_err(|error| error.into())
        }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

impl Sse {
    /// Create a paif of sender and SSE responder
    pub fn channel() -> (Sender, Sse) {
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
        response.insert_header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        response.insert_header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        response.replace_body(self.stream);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::{
        handler::into_endpoint,
        responder::{
            sse::{Event, Sender},
            Sse,
        },
        test_helper::test_serve,
    };
    use std::time::Duration;
    use tokio::task::spawn;
    use tokio::time::sleep;

    #[tokio::test]
    async fn channel_count() {
        async fn handler() -> Sse {
            let (sender, sse) = Sender::new();
            spawn(async move {
                let mut count = 1;
                loop {
                    sender.send(Event::data(count.to_string())).await.unwrap();
                    sleep(Duration::from_secs(1)).await;
                    count += 1;
                }
            });
            sse
        }
        test_serve(into_endpoint(handler)).await;
    }
}
