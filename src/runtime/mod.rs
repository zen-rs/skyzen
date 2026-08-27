//! Runtime utilities used by `#[skyzen::main]`.

/// Cloudflare's `request.cf` edge metadata. `wasm32`-only — see [`CfProperties`].
// Compiled under `test` as well so the decode regression tests run in native CI; the
// JS-touching halves inside stay wasm-gated.
#[cfg(any(target_arch = "wasm32", test))]
mod cf;
mod context;

#[cfg(any(target_arch = "wasm32", test))]
pub use cf::{CfBotManagement, CfProperties, CfPropertiesSlot, CfPropertiesUnavailable};
pub use context::{WorkerContext, WorkerContextError, WorkerContextNotConfigured};

/// Native (smol backed) runtime utilities.
#[cfg(all(not(target_arch = "wasm32"), feature = "rt"))]
pub mod native;

/// The native queue-consumer runtime that drives `#[skyzen::queue]` off the edge.
#[cfg(all(not(target_arch = "wasm32"), feature = "rt"))]
pub mod consumer;

/// The Azure Functions custom-handler integration.
#[cfg(all(not(target_arch = "wasm32"), feature = "rt"))]
pub mod azure;

/// Native test runtime utilities used by `#[skyzen::test]`.
#[cfg(not(target_arch = "wasm32"))]
pub mod testing;

/// WebWorker/WASM runtime utilities.
#[cfg(target_arch = "wasm32")]
pub mod wasm;
