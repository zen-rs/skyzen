use std::future::Future;

use executor_core::{compio::Runtime, smol::SmolGlobal, try_init_global_executor};

/// Drive an async test future to completion on a compio runtime of its own.
///
/// Background work the test spawns through the global executor runs on Skyzen's `Send` executor,
/// as it does under `#[skyzen::main]`.
///
/// # Panics
///
/// If the compio runtime cannot be created.
pub fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    let _ = try_init_global_executor(SmolGlobal);
    Runtime::new()
        .expect("a compio runtime for the test")
        .block_on(future)
}
