//! The native queue-consumer runtime.
//!
//! Cloudflare Queues *pushes* a batch into a Worker: the platform owns the polling loop and
//! Skyzen only supplies the handler. Every other backend Skyzen speaks to — SQS, Azure Service
//! Bus, Azure Storage queues, the in-memory mock — is *pull*-based, so somebody has to run that
//! loop. This module is that somebody, which is what makes `#[skyzen::queue]` dual-target: the
//! same annotated function is invoked by the platform on wasm and by [`run_consumer`] natively.
//!
//! # Delivery semantics
//!
//! Delivery is **at-least-once**. A batch is settled only after the handler returns, so a process
//! that dies mid-batch leaves its messages leased; the backend's visibility timeout then expires
//! and redelivers them. Nothing is deduplicated, ordering across concurrent slots is not
//! preserved, and a handler must therefore be idempotent.
//!
//! # What a handler sees
//!
//! The batch is a [`QueueBatch`] exactly as on Cloudflare, with two differences worth knowing:
//!
//! - [`QueueMessage::timestamp_ms`] is the moment this runtime *received* the batch. Pull-based
//!   backends do not report the enqueue time through the portable [`ReceivedMessage`], so this is
//!   a receive timestamp rather than Cloudflare's enqueue timestamp.
//! - [`QueueMessage::id`] falls back to the lease receipt on a backend that assigns no message id,
//!   because the receipt is then the only identifier the delivery has.
//!
//! Redelivery counts stay with the driver: they are recorded on the batch's log span
//! (`attempts`), not on the message, so the same handler compiles for both targets.

use core::{convert::Infallible, future::Future, num::NonZeroUsize, time::Duration};
use std::{
    panic::AssertUnwindSafe,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_channel::{Receiver, Sender};
use executor_core::{Executor as CoreExecutor, Task};
use futures_util::{
    future::{select, Either},
    FutureExt,
};
use skyzen_core::ErrorChain;
use skyzen_services::{
    queue::{
        MessageReceipt, QueueBatch, QueueBatchDisposition, QueueError, QueueMessage,
        QueueMessageDisposition, QueueRetry, ReceiveOptions, ReceivedMessage,
    },
    BoxError, Queue,
};
use tracing::{debug, error, info, warn};

/// The shortest pause after a failed receive, doubled on each consecutive failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// The polling parameters of one `[[native.queue_consumer]]` entry.
///
/// `#[skyzen::main]` builds one of these per manifest entry; the values are already validated at
/// compile time, so nothing here is re-checked at runtime.
#[derive(Debug, Clone)]
pub struct ConsumerConfig {
    /// The portable `[[service]]` name, reported to the handler as [`QueueBatch::queue`].
    pub queue: String,
    /// How many polling loops run against this queue at once.
    pub concurrency: NonZeroUsize,
    /// Most messages to take in one receive; the backend caps it further if it must.
    pub batch_size: NonZeroUsize,
    /// How long a receive may wait for a message, and the loop's idle pace.
    pub poll_wait: Duration,
    /// How long a received batch stays invisible, or the queue's own default when `None`.
    pub visibility_timeout: Option<Duration>,
    /// The retry applied to a message the handler asked to retry without naming a delay.
    pub default_retry: QueueRetry,
}

impl ConsumerConfig {
    /// The receive options every poll of this consumer uses.
    ///
    /// `wait` is always set: Azure Service Bus refuses a receive that carries none, and a backend
    /// that answers sooner is paced by the loop instead.
    fn receive_options(&self) -> ReceiveOptions {
        let options = ReceiveOptions::new()
            .with_max_messages(self.batch_size.get())
            .with_wait(self.poll_wait);

        self.visibility_timeout
            .map_or(options, |timeout| options.with_visibility_timeout(timeout))
    }
}

/// A queue handler the native consumer loop can drive.
///
/// `#[skyzen::queue]` generates the implementation: it decodes the raw batch into the handler's
/// own message type, calls the handler, and converts whatever it returns through
/// [`IntoQueueDisposition`]. Handwritten implementations are welcome to skip the macro entirely.
pub trait QueueConsumer: Clone + Send + Sync + 'static {
    /// Handle one received batch and say how its messages should be settled.
    ///
    /// # Errors
    ///
    /// Returns the handler's own error, or a decode failure, whichever came first. Either one
    /// makes the driver retry the whole batch.
    fn handle(
        &self,
        batch: QueueBatch<Vec<u8>>,
    ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send;
}

/// Convert a `#[skyzen::queue]` handler's return value into a settle decision.
///
/// The accepted shapes mirror Cloudflare's `IntoQueueWorkerResult` one for one, so a handler
/// written for the edge needs no change to run natively: `()` and `Ok(())` acknowledge the batch,
/// an `Err` retries it, and a [`QueueBatchDisposition`] settles message by message.
pub trait IntoQueueDisposition {
    /// Convert the handler output into the decision the driver applies.
    ///
    /// # Errors
    ///
    /// Returns the handler's error when it returned one.
    fn into_queue_disposition(self) -> Result<QueueBatchDisposition, BoxError>;
}

impl IntoQueueDisposition for () {
    fn into_queue_disposition(self) -> Result<QueueBatchDisposition, BoxError> {
        Ok(QueueBatchDisposition::ack_all())
    }
}

impl<E> IntoQueueDisposition for Result<(), E>
where
    E: core::error::Error + Send + Sync + 'static,
{
    fn into_queue_disposition(self) -> Result<QueueBatchDisposition, BoxError> {
        self.map(|()| QueueBatchDisposition::ack_all())
            .map_err(Into::into)
    }
}

impl IntoQueueDisposition for QueueBatchDisposition {
    fn into_queue_disposition(self) -> Result<QueueBatchDisposition, BoxError> {
        Ok(self)
    }
}

impl<E> IntoQueueDisposition for Result<QueueBatchDisposition, E>
where
    E: core::error::Error + Send + Sync + 'static,
{
    fn into_queue_disposition(self) -> Result<QueueBatchDisposition, BoxError> {
        self.map_err(Into::into)
    }
}

/// A consumer that can never run against the backend it was pointed at.
///
/// Reported through a channel rather than logged and swallowed: an application that declares a
/// consumer and silently never consumes is worse than one that refuses to start.
#[derive(Debug, Clone)]
pub struct ConsumerFatal {
    /// The `[[service]]` name of the queue that cannot be consumed.
    pub queue: String,
    /// What the backend said when asked to receive.
    pub reason: &'static str,
}

/// The queue consumers an application declares, ready for the runtime to start.
///
/// A crate has at most one `#[skyzen::queue]` handler — the Cloudflare export name is singular —
/// so every entry shares one handler and the driver stays generic over its type rather than
/// boxing it.
pub struct QueueConsumers<H> {
    handler: H,
    entries: Vec<(ConsumerConfig, Queue)>,
}

impl<H> core::fmt::Debug for QueueConsumers<H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("QueueConsumers")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl<H: QueueConsumer> QueueConsumers<H> {
    /// Declare `entries`, each a configured queue, all driven by `handler`.
    #[must_use]
    pub const fn new(handler: H, entries: Vec<(ConsumerConfig, Queue)>) -> Self {
        Self { handler, entries }
    }
}

/// The consumers an application runs beside its HTTP server.
///
/// Implemented by `()` for an application that declares none and by [`QueueConsumers`] for one
/// that does, so the runtime has a single launch path and no "no consumer" placeholder handler.
///
/// # Polling and being pushed
///
/// [`start`](Self::start) is the pull side: it owns the receive loops, and only a plain server
/// runs them. [`dispatch`](Self::dispatch) is the push side, for a platform that invokes the
/// application with a batch it already holds — AWS Lambda through an SQS event source, and the
/// Azure Functions host through a queue trigger. Both sides drive the same `#[skyzen::queue]`
/// handler; a serverless deployment uses only the second, because a polling loop inside a
/// function that scales to zero would consume messages nobody is paying attention to.
pub trait ConsumerSet: Send + Sync + 'static {
    /// Whether a `#[skyzen::queue]` handler exists at all.
    ///
    /// A const, so a platform integration can refuse a queue trigger *at startup* rather than on
    /// the first message that arrives with nothing to handle it.
    const DECLARES_HANDLER: bool;

    /// Spawn every declared consumer loop.
    ///
    /// `stop` is closed when the process begins shutting down and `guard` is a drain token: each
    /// loop holds a clone until it has finished and settled its current batch, which is what
    /// makes graceful shutdown wait for in-flight work.
    fn start<Exec: CoreExecutor + 'static>(
        self,
        executor: &Exec,
        stop: &Receiver<Infallible>,
        guard: &Sender<Infallible>,
        fatal: &Sender<ConsumerFatal>,
    );

    /// Hand a batch the platform pushed to the declared handler.
    ///
    /// Settling is the platform's job here — it holds the lease — so this only reports what the
    /// handler decided.
    ///
    /// # Errors
    ///
    /// Returns the handler's own error, or a decode failure, whichever came first.
    fn dispatch(
        &self,
        batch: QueueBatch<Vec<u8>>,
    ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send;
}

impl ConsumerSet for () {
    const DECLARES_HANDLER: bool = false;

    fn start<Exec: CoreExecutor + 'static>(
        self,
        _executor: &Exec,
        _stop: &Receiver<Infallible>,
        _guard: &Sender<Infallible>,
        _fatal: &Sender<ConsumerFatal>,
    ) {
    }

    fn dispatch(
        &self,
        _batch: QueueBatch<Vec<u8>>,
    ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send {
        // Callers check `DECLARES_HANDLER` first and refuse the trigger by name, so this is the
        // backstop for a hand-written integration that forgot to.
        core::future::ready(Err(BoxError::from(
            "this application declares no #[skyzen::queue] handler",
        )))
    }
}

impl<H: QueueConsumer> ConsumerSet for QueueConsumers<H> {
    const DECLARES_HANDLER: bool = true;

    fn dispatch(
        &self,
        batch: QueueBatch<Vec<u8>>,
    ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send {
        self.handler.handle(batch)
    }

    fn start<Exec: CoreExecutor + 'static>(
        self,
        executor: &Exec,
        stop: &Receiver<Infallible>,
        guard: &Sender<Infallible>,
        fatal: &Sender<ConsumerFatal>,
    ) {
        for (config, queue) in self.entries {
            info!(
                queue = config.queue.as_str(),
                concurrency = config.concurrency.get(),
                batch_size = config.batch_size.get(),
                poll_wait_ms = u64::try_from(config.poll_wait.as_millis()).unwrap_or(u64::MAX),
                "starting native queue consumer"
            );

            for _ in 0..config.concurrency.get() {
                let config = config.clone();
                let queue = queue.clone();
                let handler = self.handler.clone();
                let stop = stop.clone();
                let guard = guard.clone();
                let fatal = fatal.clone();
                executor
                    .spawn(async move {
                        // Held for as long as the loop runs, so a shutdown waits for the batch
                        // this slot is in the middle of settling.
                        let _guard = guard;
                        run_consumer(queue, config, handler, stop, fatal).await;
                    })
                    .detach();
            }
        }
    }
}

/// Poll `queue` until `stop` is closed, handing every batch to `handler`.
///
/// One call is one concurrency slot: a consumer configured with `concurrency = 4` runs four of
/// these over clones of the same queue.
///
/// The loop never initiates a receive after `stop` closes, and an in-flight receive is abandoned
/// rather than awaited — abandoned messages stay leased and are redelivered when their visibility
/// timeout lapses, which is the at-least-once contract this runtime already documents. A batch
/// that *has* been handed to the handler is always run to completion and settled.
pub async fn run_consumer<H: QueueConsumer>(
    queue: Queue,
    config: ConsumerConfig,
    handler: H,
    stop: Receiver<Infallible>,
    fatal: Sender<ConsumerFatal>,
) {
    let options = config.receive_options();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        // Every await in an iteration can be ready on creation — an in-memory backend's receive
        // and settle both are — so without an explicit yield a queue that always has work would
        // starve every other task sharing this executor thread.
        yield_now().await;

        // Shutdown is checked here rather than left to the race below: a receive that is ready on
        // creation always wins that race, so a backend that never has to wait would otherwise
        // keep consuming forever.
        if stop.is_closed() {
            break;
        }

        let started = Instant::now();
        let Some(received) = interruptible(&stop, queue.receive(options)).await else {
            break;
        };

        match received {
            Ok(messages) if messages.is_empty() => {
                backoff = INITIAL_BACKOFF;
                // A backend that honours `wait` has already spent the interval; one that answers
                // an empty receive immediately (the in-memory mock, or a backend whose long poll
                // returns early) would otherwise spin, so the loop idles out the remainder.
                if let Some(remainder) = config.poll_wait.checked_sub(started.elapsed()) {
                    if idle(&stop, remainder).await.is_none() {
                        break;
                    }
                }
            }
            Ok(messages) => {
                backoff = INITIAL_BACKOFF;
                process_batch(&queue, &config, &handler, messages).await;
            }
            Err(QueueError::Unsupported(reason)) => {
                // Not a transient failure: this backend has no pull-based consumption at all, so
                // no amount of retrying will produce a message.
                let _ = fatal
                    .try_send(ConsumerFatal {
                        queue: config.queue.clone(),
                        reason,
                    })
                    .inspect_err(|error| {
                        debug!(
                            queue = config.queue.as_str(),
                            "the runtime already knows this consumer cannot run: {error}"
                        );
                    });
                break;
            }
            Err(error) => {
                let delay = backoff.min(config.poll_wait);
                warn!(
                    queue = config.queue.as_str(),
                    retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    error = %ErrorChain(&error),
                    "queue receive failed; backing off"
                );
                backoff = backoff.saturating_mul(2);
                if idle(&stop, delay).await.is_none() {
                    break;
                }
            }
        }
    }

    debug!(
        queue = config.queue.as_str(),
        "native queue consumer stopped"
    );
}

/// Hand the executor back to whatever else is ready, then continue.
async fn yield_now() {
    let mut yielded = false;
    core::future::poll_fn(|context| {
        if yielded {
            core::task::Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            core::task::Poll::Pending
        }
    })
    .await;
}

/// Run `work` unless the runtime is shutting down, returning `None` once it is.
///
/// `stop` carries no messages: it resolves exactly when the runtime drops the last sender, which
/// is how one Ctrl+C reaches every consumer slot at once.
async fn interruptible<T>(stop: &Receiver<Infallible>, work: impl Future<Output = T>) -> Option<T> {
    let work = core::pin::pin!(work);
    let stopped = core::pin::pin!(stop.recv());

    match select(work, stopped).await {
        Either::Left((output, _)) => Some(output),
        Either::Right(..) => None,
    }
}

/// Sleep for `delay`, or return `None` as soon as the runtime starts shutting down.
async fn idle(stop: &Receiver<Infallible>, delay: Duration) -> Option<()> {
    interruptible(stop, async_io::Timer::after(delay))
        .await
        .map(|_deadline| ())
}

/// Hand one received batch to the handler and settle it.
async fn process_batch<H: QueueConsumer>(
    queue: &Queue,
    config: &ConsumerConfig,
    handler: &H,
    messages: Vec<ReceivedMessage>,
) {
    let attempts = messages.iter().filter_map(|message| message.attempts).max();
    let batch = build_batch(&config.queue, &messages);

    debug!(
        queue = config.queue.as_str(),
        messages = messages.len(),
        attempts,
        "handling queue batch"
    );

    // A panicking handler must not take the consumer down with it: the batch is retried the same
    // way an `Err` return is, and the next poll carries on.
    let outcome = AssertUnwindSafe(handler.handle(batch)).catch_unwind().await;

    let disposition = match outcome {
        Ok(Ok(disposition)) => disposition,
        Ok(Err(error)) => {
            error!(
                queue = config.queue.as_str(),
                messages = messages.len(),
                attempts,
                error = %ErrorChain(error.as_ref()),
                "queue handler failed; retrying the batch"
            );
            QueueBatchDisposition::retry_all(config.default_retry)
        }
        Err(panic) => {
            error!(
                queue = config.queue.as_str(),
                messages = messages.len(),
                attempts,
                panic = panic_message(panic.as_ref()),
                "queue handler panicked; retrying the batch"
            );
            QueueBatchDisposition::retry_all(config.default_retry)
        }
    };

    settle_batch(queue, config, &messages, disposition).await;
}

/// Build the portable batch a handler receives from what the backend delivered.
fn build_batch(queue: &str, messages: &[ReceivedMessage]) -> QueueBatch<Vec<u8>> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since_epoch| {
            i64::try_from(since_epoch.as_millis()).unwrap_or(i64::MAX)
        });

    QueueBatch {
        queue: queue.to_owned(),
        messages: messages
            .iter()
            .map(|message| QueueMessage {
                id: message
                    .id
                    .clone()
                    .unwrap_or_else(|| message.receipt.as_str().to_owned()),
                timestamp_ms,
                body: message.body.clone(),
            })
            .collect(),
    }
}

/// Apply the handler's decision to every message in the batch.
///
/// A settle call that fails is logged and the loop carries on: the message stays leased, so the
/// visibility timeout redelivers it, and stopping the consumer over it would turn one backend
/// hiccup into an outage.
async fn settle_batch(
    queue: &Queue,
    config: &ConsumerConfig,
    messages: &[ReceivedMessage],
    disposition: QueueBatchDisposition,
) {
    let decisions = match disposition {
        QueueBatchDisposition::All(decision) => vec![decision; messages.len()],
        QueueBatchDisposition::PerMessage(decisions) if decisions.len() == messages.len() => {
            decisions
        }
        QueueBatchDisposition::PerMessage(decisions) => {
            // Settling by index against a mismatched list would ack whichever messages happened
            // to line up, so the whole batch is retried instead.
            error!(
                queue = config.queue.as_str(),
                decisions = decisions.len(),
                messages = messages.len(),
                "queue handler returned one decision per message for a different batch size; retrying the batch"
            );
            vec![QueueMessageDisposition::Retry(config.default_retry); messages.len()]
        }
    };

    for (message, decision) in messages.iter().zip(decisions) {
        let settled = match decision {
            QueueMessageDisposition::Ack => queue.ack(&message.receipt).await,
            QueueMessageDisposition::Retry(retry) => {
                queue
                    .nack(&message.receipt, retry_or_default(retry, config))
                    .await
            }
        };

        if let Err(error) = settled {
            warn!(
                queue = config.queue.as_str(),
                message = message_label(message.id.as_deref(), &message.receipt),
                error = %ErrorChain(&error),
                "failed to settle a queue message; it will be redelivered"
            );
        }
    }
}

/// The retry the driver applies: the handler's own delay, or the configured default.
const fn retry_or_default(retry: QueueRetry, config: &ConsumerConfig) -> QueueRetry {
    if retry.delay_seconds.is_some() {
        retry
    } else {
        config.default_retry
    }
}

/// How a message is named in a log line when the backend assigns no id.
fn message_label<'a>(id: Option<&'a str>, receipt: &'a MessageReceipt) -> &'a str {
    id.unwrap_or_else(|| receipt.as_str())
}

/// The message a panicking handler carried, when it carried a printable one.
fn panic_message(panic: &(dyn core::any::Any + Send)) -> &str {
    panic.downcast_ref::<&'static str>().map_or_else(
        || {
            panic
                .downcast_ref::<String>()
                .map_or("<non-string panic payload>", String::as_str)
        },
        |message| *message,
    )
}

#[cfg(test)]
mod tests {
    use super::{run_consumer, ConsumerConfig, ConsumerFatal, IntoQueueDisposition, QueueConsumer};
    use core::{convert::Infallible, future::Future, num::NonZeroUsize, time::Duration};
    use skyzen_services::{
        queue::{
            MessageQueue, QueueBatch, QueueBatchDisposition, QueueError, QueueMessageDisposition,
            QueueRetry,
        },
        BoxError, Queue,
    };
    use skyzen_test::mock::InMemoryQueue;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    /// A queue whose `receive` reports that the backend has no pull-based consumption at all.
    #[derive(Debug, Clone, Default)]
    struct PushOnlyQueue;

    // Sending nowhere is synchronous, so the futures are ready on creation rather than `async`
    // blocks with nothing to await.
    impl MessageQueue for PushOnlyQueue {
        fn send(&self, _message: &[u8]) -> impl Future<Output = Result<(), QueueError>> + Send {
            core::future::ready(Ok(()))
        }

        fn send_batch(
            &self,
            _messages: &[Vec<u8>],
        ) -> impl Future<Output = Result<(), QueueError>> + Send {
            core::future::ready(Ok(()))
        }
    }

    fn config(queue: &str) -> ConsumerConfig {
        ConsumerConfig {
            queue: queue.to_owned(),
            concurrency: NonZeroUsize::new(1).unwrap(),
            batch_size: NonZeroUsize::new(10).unwrap(),
            poll_wait: Duration::from_millis(20),
            visibility_timeout: Some(Duration::from_millis(200)),
            default_retry: QueueRetry::new().with_delay_seconds(0),
        }
    }

    /// A handler that records every batch it saw and answers with a canned decision.
    #[derive(Clone)]
    struct Recorder {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
        calls: Arc<AtomicUsize>,
        decide: Arc<dyn Fn(usize) -> Result<QueueBatchDisposition, BoxError> + Send + Sync>,
    }

    impl Recorder {
        fn new(
            decide: impl Fn(usize) -> Result<QueueBatchDisposition, BoxError> + Send + Sync + 'static,
        ) -> Self {
            Self {
                seen: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(AtomicUsize::new(0)),
                decide: Arc::new(decide),
            }
        }

        fn batches(&self) -> Vec<Vec<String>> {
            self.seen.lock().expect("recorder lock").clone()
        }
    }

    impl QueueConsumer for Recorder {
        // Deliberately an `async` block and not an `async fn`: the work must happen when the
        // driver polls the future, inside its panic guard, not when `handle` is called.
        #[allow(clippy::manual_async_fn)]
        fn handle(
            &self,
            batch: QueueBatch<Vec<u8>>,
        ) -> impl Future<Output = Result<QueueBatchDisposition, BoxError>> + Send {
            async move {
                let bodies = batch
                    .messages
                    .iter()
                    .map(|message| String::from_utf8(message.body.clone()).expect("utf-8 body"))
                    .collect();
                self.seen.lock().expect("recorder lock").push(bodies);
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                (self.decide)(call)
            }
        }
    }

    /// A handler that reports when it starts and then takes its time, so a test can stop the
    /// runtime while a batch is genuinely in flight.
    #[derive(Clone)]
    struct SlowHandler {
        entered: async_channel::Sender<()>,
    }

    impl QueueConsumer for SlowHandler {
        async fn handle(
            &self,
            _batch: QueueBatch<Vec<u8>>,
        ) -> Result<QueueBatchDisposition, BoxError> {
            let _ = self.entered.send(()).await;
            async_io::Timer::after(Duration::from_millis(120)).await;
            Ok(QueueBatchDisposition::ack_all())
        }
    }

    /// Run a consumer until `until` reports the test has seen enough, then stop it and wait for
    /// it to finish.
    ///
    /// The consumer and the supervisor that stops it are joined on one task rather than spawned:
    /// the test then owns the whole schedule and needs no executor of its own.
    async fn drive(
        backend: InMemoryQueue,
        handler: Recorder,
        config: ConsumerConfig,
        until: impl Fn(&Recorder) -> bool + Send + Sync,
    ) -> Vec<ConsumerFatal> {
        let (stop_tx, stop_rx) = async_channel::bounded::<Infallible>(1);
        let (fatal_tx, fatal_rx) = async_channel::bounded::<ConsumerFatal>(1);
        let consumer = run_consumer(
            Queue::new(backend),
            config,
            handler.clone(),
            stop_rx,
            fatal_tx,
        );
        let supervisor = async {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !until(&handler) && std::time::Instant::now() < deadline {
                async_io::Timer::after(Duration::from_millis(1)).await;
            }
            drop(stop_tx);
        };

        futures_util::future::join(consumer, supervisor).await;

        let mut fatal = Vec::new();
        while let Ok(report) = fatal_rx.try_recv() {
            fatal.push(report);
        }
        fatal
    }

    #[test]
    fn dispositions_mirror_the_cloudflare_conversions() {
        assert_eq!(
            ().into_queue_disposition().unwrap(),
            QueueBatchDisposition::ack_all()
        );
        assert_eq!(
            Ok::<_, QueueError>(()).into_queue_disposition().unwrap(),
            QueueBatchDisposition::ack_all()
        );
        assert_eq!(
            QueueBatchDisposition::retry_all(QueueRetry::new())
                .into_queue_disposition()
                .unwrap(),
            QueueBatchDisposition::retry_all(QueueRetry::new())
        );
        let error = Err::<QueueBatchDisposition, _>(QueueError::backend("no"))
            .into_queue_disposition()
            .unwrap_err();
        assert!(error.to_string().contains("no"));
    }

    #[test]
    fn processes_a_batch_and_acknowledges_every_message() {
        async_io::block_on(async {
            let backend = InMemoryQueue::new();
            backend.send(b"one").await.unwrap();
            backend.send(b"two").await.unwrap();

            let handler = Recorder::new(|_| Ok(QueueBatchDisposition::ack_all()));
            let fatal = drive(
                backend.clone(),
                handler.clone(),
                config("jobs"),
                |handler| !handler.batches().is_empty(),
            )
            .await;

            assert!(fatal.is_empty());
            assert_eq!(
                handler.batches()[0],
                vec!["one".to_owned(), "two".to_owned()]
            );
            assert!(backend.messages().is_empty(), "acked messages are deleted");
        });
    }

    #[test]
    fn a_retried_message_comes_back_with_another_attempt() {
        async_io::block_on(async {
            let backend = InMemoryQueue::new();
            backend.send(b"retry-me").await.unwrap();

            // Retry on the first delivery, acknowledge on the second.
            let handler = Recorder::new(|call| {
                Ok(if call == 0 {
                    QueueBatchDisposition::retry_all(QueueRetry::new().with_delay_seconds(0))
                } else {
                    QueueBatchDisposition::ack_all()
                })
            });
            let fatal = drive(
                backend.clone(),
                handler.clone(),
                config("jobs"),
                |handler| handler.batches().len() >= 2,
            )
            .await;

            assert!(fatal.is_empty());
            assert_eq!(handler.batches().len(), 2, "the message was redelivered");
            assert_eq!(handler.batches()[1], vec!["retry-me".to_owned()]);
            assert!(backend.messages().is_empty(), "the retry was acked");
        });
    }

    #[test]
    fn a_failing_handler_retries_the_whole_batch() {
        async_io::block_on(async {
            let backend = InMemoryQueue::new();
            backend.send(b"poison").await.unwrap();

            let handler = Recorder::new(|_| Err(QueueError::backend("handler exploded").into()));
            let fatal = drive(
                backend.clone(),
                handler.clone(),
                config("jobs"),
                |handler| handler.batches().len() >= 2,
            )
            .await;

            assert!(fatal.is_empty());
            // Redelivered rather than dropped, and still on the queue when the consumer stops.
            assert!(handler.batches().len() >= 2);
            let queued = backend.messages();
            assert_eq!(queued.len(), 1);
            assert_eq!(queued[0], b"poison".to_vec());
        });
    }

    #[test]
    fn a_panicking_handler_retries_the_batch_and_keeps_polling() {
        async_io::block_on(async {
            let backend = InMemoryQueue::new();
            backend.send(b"boom").await.unwrap();

            let handler = Recorder::new(|call| {
                assert!(call > 0, "the first delivery panics");
                Ok(QueueBatchDisposition::ack_all())
            });
            let fatal = drive(
                backend.clone(),
                handler.clone(),
                config("jobs"),
                |handler| handler.batches().len() >= 2,
            )
            .await;

            assert!(fatal.is_empty());
            assert!(backend.messages().is_empty(), "the retry was acked");
        });
    }

    #[test]
    fn a_mismatched_per_message_decision_retries_rather_than_settling_by_index() {
        async_io::block_on(async {
            let backend = InMemoryQueue::new();
            backend.send(b"a").await.unwrap();
            backend.send(b"b").await.unwrap();

            let handler = Recorder::new(|call| {
                Ok(if call == 0 {
                    QueueBatchDisposition::PerMessage(vec![QueueMessageDisposition::Ack])
                } else {
                    QueueBatchDisposition::ack_all()
                })
            });
            let fatal = drive(
                backend.clone(),
                handler.clone(),
                config("jobs"),
                |handler| handler.batches().len() >= 2,
            )
            .await;

            assert!(fatal.is_empty());
            assert_eq!(handler.batches()[1].len(), 2, "neither message was acked");
            assert_eq!(backend.messages().len(), 0, "the retry was acked");
        });
    }

    #[test]
    fn a_push_only_backend_reports_a_fatal_consumer() {
        async_io::block_on(async {
            let (stop_tx, stop_rx) = async_channel::bounded::<Infallible>(1);
            let (fatal_tx, fatal_rx) = async_channel::bounded::<ConsumerFatal>(1);
            let handler = Recorder::new(|_| Ok(QueueBatchDisposition::ack_all()));

            run_consumer(
                Queue::new(PushOnlyQueue),
                config("pushed"),
                handler.clone(),
                stop_rx,
                fatal_tx,
            )
            .await;
            drop(stop_tx);

            let fatal = fatal_rx
                .try_recv()
                .expect("the consumer reports it cannot run");
            assert_eq!(fatal.queue, "pushed");
            assert!(fatal.reason.contains("pull-based"));
            assert_eq!(
                handler.batches().len(),
                0,
                "no batch ever reached the handler"
            );
        });
    }

    #[test]
    fn shutdown_finishes_and_settles_the_batch_in_flight() {
        async_io::block_on(async {
            let backend = InMemoryQueue::new();
            backend.send(b"slow").await.unwrap();

            let (stop_tx, stop_rx) = async_channel::bounded::<Infallible>(1);
            let (fatal_tx, _fatal_rx) = async_channel::bounded::<ConsumerFatal>(1);
            let (entered_tx, entered_rx) = async_channel::bounded::<()>(1);

            let consumer = run_consumer(
                Queue::new(backend.clone()),
                config("jobs"),
                SlowHandler {
                    entered: entered_tx,
                },
                stop_rx,
                fatal_tx,
            );
            let supervisor = async {
                // Stop the runtime while the handler is still working on its batch.
                entered_rx.recv().await.expect("the handler started");
                drop(stop_tx);
            };

            futures_util::future::join(consumer, supervisor).await;

            assert_eq!(
                backend.messages().len(),
                0,
                "the in-flight batch was settled before the consumer stopped"
            );
        });
    }
}
