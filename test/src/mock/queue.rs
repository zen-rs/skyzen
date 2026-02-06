//! In-memory message queue for testing.

use std::{collections::VecDeque, sync::Arc, sync::RwLock};

use skyzen_services::queue::{MessageQueue, QueueError};

/// An in-memory message queue backed by a `VecDeque`.
///
/// Each instance starts empty and is completely isolated.
/// Designed for use in tests where each test gets a fresh instance.
///
/// Messages can be inspected after test operations using [`messages`](Self::messages).
#[derive(Debug, Clone)]
pub struct InMemoryQueue {
    data: Arc<RwLock<VecDeque<Vec<u8>>>>,
}

impl InMemoryQueue {
    /// Create a new empty in-memory queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(VecDeque::new())),
        }
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
        self.data
            .write()
            .expect("InMemoryQueue lock poisoned")
            .push_back(message.to_vec());
        Ok(())
    }

    async fn send_batch(&self, messages: &[Vec<u8>]) -> Result<(), QueueError> {
        self.data
            .write()
            .expect("InMemoryQueue lock poisoned")
            .extend(messages.iter().cloned());
        Ok(())
    }
}
