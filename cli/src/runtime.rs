//! The async runtime the CLI drives its own clients on.
//!
//! Nothing in the CLI is async: it is a program that runs a handful of requests and exits. The
//! parts that talk to a cloud API or a database do so through crates that are — the AWS SDK, the
//! ARM client, sqlx — so each of them borrows one current-thread runtime for the duration of the
//! call and gives it back. A thread pool would be pure startup cost for a handful of round trips,
//! and a runtime built per call site would be the same three lines written four times.

use anyhow::{Context, Result};
use std::future::Future;

/// Run one future to completion on a current-thread runtime built for it.
///
/// `enable_all` is what gives the drivers underneath a reactor and a timer; without it a TCP
/// connection or a waiter's sleep panics rather than failing.
///
/// # Errors
///
/// Fails when the runtime cannot be built, which on a healthy machine means the process is out of
/// file descriptors.
pub fn block_on<F: Future>(future: F) -> Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the async runtime the CLI's clients need")?;
    Ok(runtime.block_on(future))
}
