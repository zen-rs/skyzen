//! Runtime utilities used by `#[skyzen::main]`.

#[cfg(not(target_arch = "wasm32"))]
/// Tokio-backed runtime utilities.
pub mod native;

#[cfg(target_arch = "wasm32")]
/// WebWorker/WASM runtime utilities.
pub mod wasm;
