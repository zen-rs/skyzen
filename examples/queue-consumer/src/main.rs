//! A native queue consumer: one process that accepts jobs over HTTP and works them off a queue.
//!
//! The interesting part is what is *not* here. There is no polling loop, no ack/nack bookkeeping
//! and no shutdown handling: `[[native.queue_consumer]]` in `Skyzen.toml` tells `#[skyzen::main]`
//! to run all of that, and `#[skyzen::queue]` — the same annotation a Cloudflare Worker uses for
//! its pushed batches — is the handler it drives.
//!
//! Run it with:
//!
//! ```sh
//! cargo run -p skyzen-example-queue-consumer -- --port 3000
//! curl -X POST localhost:3000/jobs -H 'content-type: application/json' \
//!      -d '{"id":"1","action":"ship"}'
//! curl -X POST localhost:3000/jobs -H 'content-type: application/json' \
//!      -d '{"id":"2","action":"retry"}'
//! ```
//!
//! The first job is acknowledged, and the second comes back two seconds later — the
//! `retry_delay` the manifest configured — until it is asked to do something else.

use serde::{Deserialize, Serialize};
use skyzen::{
    routing::{CreateRouteNode, Route, Router},
    utils::Json,
    Result, ToSchema,
};
use skyzen_services::{QueueBatch, QueueBatchDisposition, QueueMessageDisposition, QueueRetry};

/// One unit of work, as it travels through the queue.
///
/// `ToSchema` rides along with the serde derives because the job arrives as a `Json<Job>` body:
/// the payload of a body extractor is what the generated OpenAPI document describes.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
struct Job {
    /// Caller-assigned identity, echoed in the logs.
    id: String,
    /// What to do with it. `retry` asks the consumer to hand it back to the queue.
    action: String,
}

/// Enqueue a job.
///
/// `Jobs` is generated from the `[[service]] name = "jobs"` entry and derefs to the portable
/// `Queue`. It is the very instance the consumer polls: `#[skyzen::main]` builds each declared
/// service once and shares it, which is what makes the in-memory backend work end to end here.
async fn enqueue(jobs: Jobs, Json(job): Json<Job>) -> Result<&'static str> {
    jobs.send_json(&job).await?;
    Ok("queued")
}

#[skyzen::main]
fn app() -> Router {
    Route::new(("/jobs".post(enqueue),)).build()
}

/// Work off one batch.
///
/// Natively this is called by Skyzen's consumer loop; on Cloudflare the platform calls it with a
/// pushed batch. The signature and the body are the same either way.
#[skyzen::queue]
async fn queue(batch: QueueBatch<Job>) -> QueueBatchDisposition {
    let decisions = batch
        .messages
        .iter()
        .map(|message| {
            let job = &message.body;
            if job.action == "retry" {
                tracing::warn!(job = job.id, "job asked to be retried");
                // No delay of its own, so the consumer applies the manifest's `retry_delay`.
                QueueMessageDisposition::Retry(QueueRetry::new())
            } else {
                tracing::info!(job = job.id, action = job.action, "job done");
                QueueMessageDisposition::Ack
            }
        })
        .collect();

    QueueBatchDisposition::PerMessage(decisions)
}
