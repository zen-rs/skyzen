//! In-memory message queue for testing.

use std::{
    collections::VecDeque,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use skyzen_services::queue::{
    MessageQueue, MessageReceipt, QueueError, QueueRetry, ReceiveOptions, ReceivedMessage,
    SendOptions,
};

/// How long a received message stays invisible when the caller does not say.
///
/// Matches the SQS default, so a test that omits `visibility_timeout` sees the same window a
/// default SQS queue would give it.
const DEFAULT_VISIBILITY_TIMEOUT: Duration = Duration::from_secs(30);

/// A queued message together with its delivery state.
#[derive(Debug, Clone)]
struct Queued {
    id: u64,
    body: Vec<u8>,
    /// The instant this message becomes visible: a send delay, a visibility lease, or a nack
    /// backoff all push it forward.
    visible_at: Instant,
    /// How many times the message has been delivered.
    attempts: u32,
    /// The lease held by the current consumer, if any. Cleared once the lease lapses.
    receipt: Option<u64>,
}

impl Queued {
    fn is_visible(&self, now: Instant) -> bool {
        now >= self.visible_at
    }
}

/// The mutable state behind [`InMemoryQueue`], kept under one lock so a receive-and-lease is
/// atomic the way a real broker's is.
#[derive(Debug, Default)]
struct QueueState {
    messages: VecDeque<Queued>,
    /// Monotonic source for message ids and lease receipts, so a receipt is never reused.
    next_token: u64,
}

impl QueueState {
    const fn take_token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    /// Find the message holding `receipt`, if that lease is still the current one.
    fn position_of(&self, receipt: &MessageReceipt) -> Option<usize> {
        let token: u64 = receipt.as_str().parse().ok()?;
        self.messages
            .iter()
            .position(|message| message.receipt == Some(token))
    }
}

/// An in-memory message queue backed by a `VecDeque`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
///
/// Consumption is modelled the way a pull-based broker works rather than as a plain pop:
/// [`receive`](MessageQueue::receive) leases messages and makes them invisible for the visibility
/// timeout, [`ack`](MessageQueue::ack) deletes them, [`nack`](MessageQueue::nack) reschedules them
/// after the retry delay, and every delivery bumps `attempts` (the first delivery reports `1`).
/// A lease that lapses without being settled makes the message visible again. Messages sent with
/// [`SendOptions::delay`] stay invisible until the delay elapses.
///
/// Time is read from the clock, never slept on: [`ReceiveOptions::wait`] returns immediately with
/// whatever is visible, because the mock has no runtime to long-poll with. Drive expiry in tests
/// with a zero or very short timeout rather than by sleeping.
///
/// Messages can be inspected after test operations using [`messages`](Self::messages).
///
/// Use [`fail_next_with`](Self::fail_next_with) to make the next operation
/// fail, e.g. to exercise handler error paths.
#[derive(Debug, Clone)]
pub struct InMemoryQueue {
    state: Arc<RwLock<QueueState>>,
    fail_next: Arc<RwLock<Option<String>>>,
}

impl InMemoryQueue {
    /// Create a new empty in-memory queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(QueueState::default())),
            fail_next: Arc::new(RwLock::new(None)),
        }
    }

    /// Make exactly the next queue operation fail with
    /// [`QueueError::Backend`] carrying `message`.
    ///
    /// Subsequent operations succeed again. Useful for testing how handlers
    /// react to backend failures (typically a 500 response).
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    pub fn fail_next_with(&self, message: &str) {
        *self.fail_next.write().expect("InMemoryQueue lock poisoned") = Some(message.to_owned());
    }

    fn take_injected_failure(&self) -> Result<(), QueueError> {
        let mut slot = self.fail_next.write().expect("InMemoryQueue lock poisoned");
        slot.take().map_or(Ok(()), |message| {
            drop(slot);
            Err(QueueError::backend(message))
        })
    }

    /// Enqueue one message, visible after `delay`.
    fn push(&self, body: &[u8], delay: Option<Duration>) {
        let visible_at = delay
            .and_then(|delay| Instant::now().checked_add(delay))
            .unwrap_or_else(Instant::now);
        let mut state = self.state.write().expect("InMemoryQueue lock poisoned");
        let id = state.take_token();
        state.messages.push_back(Queued {
            id,
            body: body.to_vec(),
            visible_at,
            attempts: 0,
            receipt: None,
        });
    }

    /// Return every message still in the queue, in order, whether visible or currently leased.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn messages(&self) -> Vec<Vec<u8>> {
        let state = self.state.read().expect("InMemoryQueue lock poisoned");
        state
            .messages
            .iter()
            .map(|message| message.body.clone())
            .collect()
    }

    /// Return the number of messages still in the queue, leased ones included.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        let state = self.state.read().expect("InMemoryQueue lock poisoned");
        state.messages.len()
    }

    /// Check if the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove and return the message at the front of the queue, ignoring visibility and leases.
    ///
    /// This is the blunt inspection helper; use [`receive`](MessageQueue::receive) plus
    /// [`ack`](MessageQueue::ack) to exercise the consumption path itself.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.state.write().expect("InMemoryQueue lock poisoned");
        state.messages.pop_front().map(|message| message.body)
    }
}

impl Default for InMemoryQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageQueue for InMemoryQueue {
    async fn send(&self, message: &[u8]) -> Result<(), QueueError> {
        self.take_injected_failure()?;
        self.push(message, None);
        Ok(())
    }

    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        self.take_injected_failure()?;
        for message in messages {
            self.push(message, None);
        }
        Ok(())
    }

    async fn send_with(&self, message: &[u8], options: SendOptions) -> Result<(), QueueError> {
        self.take_injected_failure()?;
        self.push(message, options.delay);
        Ok(())
    }

    async fn receive(&self, options: ReceiveOptions) -> Result<Vec<ReceivedMessage>, QueueError> {
        self.take_injected_failure()?;
        let now = Instant::now();
        let visibility = options
            .visibility_timeout
            .unwrap_or(DEFAULT_VISIBILITY_TIMEOUT);
        let lease_until = now.checked_add(visibility).unwrap_or(now);

        let mut state = self.state.write().expect("InMemoryQueue lock poisoned");
        let mut received = Vec::new();

        for index in 0..state.messages.len() {
            if received.len() >= options.max_messages {
                break;
            }
            if !state.messages[index].is_visible(now) {
                continue;
            }

            let token = state.take_token();
            let message = &mut state.messages[index];
            message.attempts += 1;
            message.receipt = Some(token);
            message.visible_at = lease_until;

            received.push(ReceivedMessage {
                id: Some(message.id.to_string()),
                body: message.body.clone(),
                receipt: MessageReceipt::new(token.to_string()),
                attempts: Some(message.attempts),
            });
        }

        drop(state);
        Ok(received)
    }

    async fn ack(&self, receipt: &MessageReceipt) -> Result<(), QueueError> {
        self.take_injected_failure()?;
        let mut state = self.state.write().expect("InMemoryQueue lock poisoned");
        let Some(index) = state.position_of(receipt) else {
            // An unknown receipt means the lease already lapsed and the message went back to the
            // queue, which is exactly the case a consumer must not treat as a successful delete.
            return Err(QueueError::Conflict);
        };
        state.messages.remove(index);
        drop(state);
        Ok(())
    }

    async fn nack(&self, receipt: &MessageReceipt, retry: QueueRetry) -> Result<(), QueueError> {
        self.take_injected_failure()?;
        let now = Instant::now();
        let visible_at = retry
            .delay_seconds
            .and_then(|delay| now.checked_add(Duration::from_secs(u64::from(delay))))
            .unwrap_or(now);

        let mut state = self.state.write().expect("InMemoryQueue lock poisoned");
        let Some(index) = state.position_of(receipt) else {
            return Err(QueueError::Conflict);
        };
        let message = &mut state.messages[index];
        message.receipt = None;
        message.visible_at = visible_at;
        drop(state);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use skyzen_services::{
        queue::{
            MessageQueue, MessageReceipt, QueueError, QueueRetry, ReceiveOptions, SendOptions,
        },
        Queue,
    };

    use super::InMemoryQueue;

    #[tokio::test]
    async fn send_and_send_batch_preserve_fifo_order_through_pop() {
        let queue = InMemoryQueue::new();
        queue.send(b"first").await.unwrap();
        queue
            .send_batch(&[b"second".to_vec(), b"third".to_vec()])
            .await
            .unwrap();
        queue.send(b"fourth").await.unwrap();

        assert_eq!(queue.len(), 4);
        assert_eq!(
            queue.messages(),
            vec![
                b"first".to_vec(),
                b"second".to_vec(),
                b"third".to_vec(),
                b"fourth".to_vec(),
            ]
        );

        assert_eq!(queue.pop(), Some(b"first".to_vec()));
        assert_eq!(queue.pop(), Some(b"second".to_vec()));
        assert_eq!(queue.pop(), Some(b"third".to_vec()));
        assert_eq!(queue.pop(), Some(b"fourth".to_vec()));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn receive_leases_a_message_and_ack_deletes_it() {
        let queue = InMemoryQueue::new();
        queue.send(b"job").await.unwrap();

        let received = queue.receive(ReceiveOptions::new()).await.unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].body, b"job".to_vec());
        assert_eq!(received[0].attempts, Some(1));

        // The leased message is invisible to the next consumer.
        assert!(queue
            .receive(ReceiveOptions::new())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(queue.len(), 1);

        queue.ack(&received[0].receipt).await.unwrap();
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_lapsed_lease_redelivers_the_message_with_a_higher_attempt_count() {
        let queue = InMemoryQueue::new();
        queue.send(b"job").await.unwrap();

        let first = queue
            .receive(ReceiveOptions::new().with_visibility_timeout(Duration::ZERO))
            .await
            .unwrap();
        assert_eq!(first[0].attempts, Some(1));

        let second = queue.receive(ReceiveOptions::new()).await.unwrap();
        assert_eq!(second[0].attempts, Some(2));
        assert_ne!(second[0].receipt, first[0].receipt);

        // The stale receipt no longer settles anything.
        assert!(matches!(
            queue.ack(&first[0].receipt).await.unwrap_err(),
            QueueError::Conflict
        ));
    }

    #[tokio::test]
    async fn nack_reschedules_the_message_and_keeps_the_attempt_count() {
        let queue = InMemoryQueue::new();
        queue.send(b"job").await.unwrap();

        let received = queue.receive(ReceiveOptions::new()).await.unwrap();
        queue
            .nack(&received[0].receipt, QueueRetry::new())
            .await
            .unwrap();

        let retried = queue.receive(ReceiveOptions::new()).await.unwrap();
        assert_eq!(retried[0].body, b"job".to_vec());
        assert_eq!(retried[0].attempts, Some(2));
    }

    #[tokio::test]
    async fn nack_with_a_delay_holds_the_message_back() {
        let queue = InMemoryQueue::new();
        queue.send(b"job").await.unwrap();

        let received = queue.receive(ReceiveOptions::new()).await.unwrap();
        queue
            .nack(
                &received[0].receipt,
                QueueRetry::new().with_delay_seconds(60),
            )
            .await
            .unwrap();

        assert!(queue
            .receive(ReceiveOptions::new())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(queue.len(), 1);
    }

    #[tokio::test]
    async fn send_with_delay_holds_the_message_until_it_is_due() {
        let queue = InMemoryQueue::new();
        queue
            .send_with(
                b"later",
                SendOptions::new().with_delay(Duration::from_mins(5)),
            )
            .await
            .unwrap();
        queue
            .send_with(b"now", SendOptions::new().with_delay(Duration::ZERO))
            .await
            .unwrap();

        let received = queue
            .receive(ReceiveOptions::new().with_max_messages(10))
            .await
            .unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].body, b"now".to_vec());
    }

    #[tokio::test]
    async fn max_messages_bounds_one_receive() {
        let queue = InMemoryQueue::new();
        queue
            .send_batch(&[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()])
            .await
            .unwrap();

        let received = queue
            .receive(ReceiveOptions::new().with_max_messages(2))
            .await
            .unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].body, b"a".to_vec());
        assert_eq!(received[1].body, b"b".to_vec());
    }

    #[tokio::test]
    async fn settling_an_unknown_receipt_is_a_conflict() {
        let queue = InMemoryQueue::new();
        let receipt = MessageReceipt::new("never-issued");

        assert!(matches!(
            queue.ack(&receipt).await.unwrap_err(),
            QueueError::Conflict
        ));
        assert!(matches!(
            queue.nack(&receipt, QueueRetry::new()).await.unwrap_err(),
            QueueError::Conflict
        ));
    }

    #[tokio::test]
    async fn wrapper_receives_and_decodes_json_bodies() {
        #[derive(Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
        struct Job {
            kind: String,
        }

        let backend = InMemoryQueue::new();
        let queue = Queue::new(backend.clone());
        queue
            .send_json(&Job {
                kind: "email".to_owned(),
            })
            .await
            .unwrap();

        let received = queue
            .receive_json::<Job>(ReceiveOptions::new())
            .await
            .unwrap();
        assert_eq!(received[0].body.kind, "email");

        queue.ack(&received[0].receipt).await.unwrap();
        assert!(backend.is_empty());
    }

    #[tokio::test]
    async fn fail_next_with_fails_exactly_one_operation() {
        let queue = InMemoryQueue::new();
        queue.fail_next_with("queue unreachable");

        let error = queue.send(b"lost").await.unwrap_err();
        assert!(
            matches!(&error, QueueError::Backend { message, .. } if message == "queue unreachable")
        );

        queue.send(b"delivered").await.unwrap();
        assert_eq!(queue.messages(), vec![b"delivered".to_vec()]);
    }
}
