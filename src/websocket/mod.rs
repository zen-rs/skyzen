//! WebSocket module entry-point.
//!
//! Native (tokio) support lives in `native.rs`, while wasm builds re-export a
//! stub implementation that makes the limitations explicit: server-side
//! upgrades and raw-socket wiring are not available on wasm targets.

mod types;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

pub use types::*;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;
