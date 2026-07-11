//! Runtime utilities used by `#[skyzen::main]`.

/// Native (smol backed) runtime utilities.
#[cfg(all(not(target_arch = "wasm32"), feature = "rt"))]
pub mod native;

/// Native test runtime utilities used by `#[skyzen::test]`.
#[cfg(not(target_arch = "wasm32"))]
pub mod testing;

/// WebWorker/WASM runtime utilities.
#[cfg(target_arch = "wasm32")]
pub mod wasm;
