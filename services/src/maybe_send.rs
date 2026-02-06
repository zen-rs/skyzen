//! Conditional `Send` bounds for cross-platform compatibility.
//!
//! On native targets (multi-threaded), futures must be `Send`.
//! On WASM (single-threaded), `Send` is not required (and JS types are `!Send`).

/// A trait that is `Send` on native targets and universally implemented on WASM.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send> MaybeSend for T {}

/// A trait that is universally implemented on WASM (no `Send` requirement).
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSend for T {}

/// Boxed future type that is `Send` on native and `!Send` on WASM.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = futures_core::future::BoxFuture<'a, T>;

/// Boxed future type that does not require `Send` on WASM.
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = futures_core::future::LocalBoxFuture<'a, T>;
