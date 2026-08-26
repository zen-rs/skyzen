//! In-memory message queue for testing.

use std::{collections::VecDeque, sync::Arc, sync::RwLock};

use skyzen_services::queue::{MessageQueue, QueueError};

/// An in-memory message queue backed by a `VecDeque`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
///
/// Messages can be inspected after test operations using [`messages`](Self::messages).
///
/// Use [`fail_next_with`](Self::fail_next_with) to make the next operation
/// fail, e.g. to exercise handler error paths.
#[derive(Debug, Clone)]
pub struct InMemoryQueue {
    data: Arc<RwLock<VecDeque<Vec<u8>>>>,
    fail_next: Arc<RwLock<Option<String>>>,
}

impl InMemoryQueue {
    /// Create a new empty in-memory queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(VecDeque::new())),
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

    /// Return all messages currently in the queue (for test assertions).
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn messages(&self) -> Vec<Vec<u8>> {
        let data = self.data.read().expect("InMemoryQueue lock poisoned");
        data.iter().cloned().collect()
    }

    /// Return the number of messages in the queue.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        let data = self.data.read().expect("InMemoryQueue lock poisoned");
        data.len()
    }

    /// Check if the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pop the next message from the front of the queue.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn pop(&self) -> Option<Vec<u8>> {
        let mut data = self.data.write().expect("InMemoryQueue lock poisoned");
        data.pop_front()
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
        self.data
            .write()
            .expect("InMemoryQueue lock poisoned")
            .push_back(message.to_vec());
        Ok(())
    }

    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        self.take_injected_failure()?;
        self.data
            .write()
            .expect("InMemoryQueue lock poisoned")
            .extend(messages.iter().cloned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use skyzen_services::queue::{MessageQueue, QueueError};

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
