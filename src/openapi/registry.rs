//! Where every `#[skyzen::openapi]` handler's metadata is collected, and the only place in the
//! crate that asks what target it is building for.
//!
//! Two backends, chosen by target, because they do not cost the same:
//!
//! - **Native uses [`linkme`], and pays nothing.** A [`HandlerSpec`] is a `static` placed in a
//!   dedicated linker section; the linker lays the section out, no code runs before `main`, and
//!   reading the registry is reading a slice. Registration is free at runtime and the specs of
//!   handlers nothing routes are still just data the linker already had to place.
//! - **wasm32 uses [`inventory`], and pays for it.** `linkme` has no WebAssembly backend — its
//!   supported-platform table is Linux, macOS, Windows, FreeBSD, OpenBSD and illumos — so the edge
//!   has to fall back on life-before-main constructors, one per documented handler, each pushing
//!   onto a linked list when the module initializes. That is real startup work and real code size,
//!   which is why it is confined here rather than adopted everywhere: an isolate's cold start
//!   absorbs a handful of pointer pushes, and the alternative on the edge is having no document at
//!   all.
//!
//! **The public surface is identical on every target**: [`iter`], and
//! [`__register_handler_spec!`](crate::__register_handler_spec) with one invocation shape. Which
//! backend answers is a private matter of this file — the `#[skyzen::openapi]` expansion, the rest
//! of the crate and an application's own code all name the same items whatever they compile for.

use super::HandlerSpec;

/// Every handler specification this binary registered, in no particular order.
pub fn iter() -> impl Iterator<Item = &'static HandlerSpec> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        backend::HANDLER_SPECS.iter()
    }

    #[cfg(target_arch = "wasm32")]
    {
        backend::inventory::iter::<HandlerSpec>()
    }
}

/// The chosen backend's plumbing.
///
/// Public because [`__register_handler_spec!`](crate::__register_handler_spec) expands in the
/// application's crate and has to name it from there, and hidden because naming it from anywhere
/// else is a mistake: these are the items that genuinely differ by target, and keeping them behind
/// one door is what lets everything above be the same everywhere.
#[doc(hidden)]
pub mod backend {
    #[cfg(not(target_arch = "wasm32"))]
    pub use linkme;

    #[cfg(target_arch = "wasm32")]
    pub use inventory;

    /// The linker section every native `#[skyzen::openapi]` handler's specification lands in.
    #[cfg(not(target_arch = "wasm32"))]
    #[linkme::distributed_slice]
    #[linkme(crate = ::skyzen::openapi::registry::backend::linkme)]
    pub static HANDLER_SPECS: [super::HandlerSpec] = [..];

    #[cfg(target_arch = "wasm32")]
    inventory::collect!(super::HandlerSpec);
}

/// Register one handler's specification with the registry.
///
/// Called only from the `#[skyzen::openapi]` expansion, which builds the [`HandlerSpec`] and knows
/// nothing about how it is stored.
///
/// Defined twice, once per target, rather than once emitting `#[cfg]`-guarded code: the two
/// backends want genuinely different item shapes — a named `static` carrying attributes for
/// `linkme`, a bare expression for `inventory` — and choosing here means the *caller* sees one
/// macro with one invocation shape, and the code it expands to carries no `#[cfg]` of its own.
#[cfg(not(target_arch = "wasm32"))]
#[doc(hidden)]
#[macro_export]
macro_rules! __register_handler_spec {
    ($ident:ident, $spec:expr) => {
        #[$crate::openapi::registry::backend::linkme::distributed_slice(
            $crate::openapi::registry::backend::HANDLER_SPECS
        )]
        #[linkme(crate = $crate::openapi::registry::backend::linkme)]
        static $ident: $crate::openapi::HandlerSpec = $spec;
    };
}

/// Register one handler's specification with the registry.
///
/// See the native definition above; this is the same macro for the target whose registry is
/// `inventory`, and takes the same arguments. The identifier is unused here — `inventory` names
/// its own shim — and is accepted so that one invocation compiles for both.
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
#[macro_export]
macro_rules! __register_handler_spec {
    ($ident:ident, $spec:expr) => {
        $crate::openapi::registry::backend::inventory::submit! { $spec }
    };
}
