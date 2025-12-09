//! Runtime utilities used by `#[skyzen::main]`.

#[cfg_attr(
    not(target_arch = "wasm32"),
    doc = "DefaultExecutor-backed runtime utilities."
)]
#[cfg(not(target_arch = "wasm32"))]
pub mod native;

#[cfg_attr(target_arch = "wasm32", doc = "WebWorker/WASM runtime utilities.")]
#[cfg(target_arch = "wasm32")]
pub mod wasm;
