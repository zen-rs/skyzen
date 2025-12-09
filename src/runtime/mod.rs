//! Runtime utilities used by `#[skyzen::main]`.

/// Native (async-executor backed) runtime utilities.
#[cfg(not(target_arch = "wasm32"))]
pub mod native;

/// WebWorker/WASM runtime utilities.
#[cfg(target_arch = "wasm32")]
pub mod wasm;
