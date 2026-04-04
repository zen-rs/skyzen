use std::future::Future;

use executor_core::{smol::SmolGlobal, try_init_global_executor};

/// Drive an async test future to completion on Skyzen's native executor.
pub fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    let _ = try_init_global_executor(SmolGlobal);
    smol::block_on(future)
}
