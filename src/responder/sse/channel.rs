use std::{
    pin::Pin,
    task::{ready, Context, Poll},
};

use async_channel::unbounded;
use http_kit::utils::Stream;
use pin_project_lite::pin_project;

use super::{Event, Sse};

/// Sender of SSE channel.
/// # Warning
/// If you don't return SSE responder in your handler.`send` method will keep await.And event stream cannot start.
#[derive(Debug, Clone)]
pub struct Sender {
    sender: async_channel::Sender<Event>,
}

impl_error!(
    SendError,
    "Error type that fails to send an event",
    "Cannot send the event to stream"
);

pin_project! {
    struct Receiver{
        #[pin]
        receiver:async_channel::Receiver<Event>,
    }
}

impl Receiver {
    pub fn new(receiver: async_channel::Receiver<Event>) -> Self {
        Self { receiver }
    }
}

impl Stream for Receiver {
    type Item = Result<Event, anyhow::Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(ready!(self.project().receiver.poll_next(cx)).map(Ok))
    }
}

impl Sender {
    pub(crate) fn new() -> (Self, Sse) {
        let (sender, receiver) = unbounded();
        (Self { sender }, Sse::from_stream(Receiver::new(receiver)))
    }

    /// Send an event to the stream.
    pub async fn send(&self, event: Event) -> Result<(), SendError> {
        self.sender.send(event).await.map_err(|_| SendError)
    }

    /// Send an event with a data payload to the stream.
    pub async fn send_data(&self, data: impl AsRef<str>) -> Result<(), SendError> {
        self.send(Event::data(data)).await
    }
}
