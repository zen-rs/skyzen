//! Runtime utilities used by `#[skyzen::main]`.

/// Cloudflare's `request.cf` edge metadata. `wasm32`-only — see [`CfProperties`].
#[cfg(target_arch = "wasm32")]
mod cf;
mod context;

#[cfg(target_arch = "wasm32")]
pub use cf::{CfBotManagement, CfProperties, CfPropertiesSlot, CfPropertiesUnavailable};
pub use context::{WorkerContext, WorkerContextError, WorkerContextNotConfigured};

/// Native (smol backed) runtime utilities.
#[cfg(all(not(target_arch = "wasm32"), feature = "rt"))]
pub mod native;

/// Native test runtime utilities used by `#[skyzen::test]`.
#[cfg(not(target_arch = "wasm32"))]
pub mod testing;

/// WebWorker/WASM runtime utilities.
#[cfg(target_arch = "wasm32")]
pub mod wasm;
