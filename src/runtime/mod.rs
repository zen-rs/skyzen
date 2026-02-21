//! Runtime utilities used by `#[skyzen::main]`.

/// Native (smol backed) runtime utilities.
#[cfg(all(not(target_arch = "wasm32"), feature = "rt"))]
pub mod native;

/// WebWorker/WASM runtime utilities.
#[cfg(target_arch = "wasm32")]
pub mod wasm;
