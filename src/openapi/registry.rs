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
//! Both halves expose the same two things — [`iter`] and
//! [`__register_handler_spec!`](crate::__register_handler_spec) — so nothing else in the workspace,
//! the `#[skyzen::openapi]` expansion included, has to know which one it is talking to.

use super::HandlerSpec;

#[cfg(not(target_arch = "wasm32"))]
pub use linkme;

#[cfg(target_arch = "wasm32")]
pub use inventory;

/// The linker-collected registry every native `#[skyzen::openapi]` handler registers into.
#[cfg(not(target_arch = "wasm32"))]
#[linkme::distributed_slice]
#[linkme(crate = ::skyzen::openapi::registry::linkme)]
pub static HANDLER_SPECS: [HandlerSpec] = [..];

#[cfg(target_arch = "wasm32")]
inventory::collect!(HandlerSpec);

/// Every handler specification this binary registered, in no particular order.
pub fn iter() -> impl Iterator<Item = &'static HandlerSpec> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        HANDLER_SPECS.iter()
    }

    #[cfg(target_arch = "wasm32")]
    {
        inventory::iter::<HandlerSpec>()
    }
}

/// Register one handler's specification with whichever registry this target uses.
///
/// Called only from the `#[skyzen::openapi]` expansion, which builds the [`HandlerSpec`] and knows
/// nothing about how it is stored — the two backends differ in the *shape* of a registration (a
/// named `static` for `linkme`, an expression for `inventory`), and that difference stops here.
#[doc(hidden)]
#[macro_export]
macro_rules! __register_handler_spec {
    ($ident:ident, $spec:expr) => {
        #[cfg(not(target_arch = "wasm32"))]
        #[$crate::openapi::registry::linkme::distributed_slice(
            $crate::openapi::registry::HANDLER_SPECS
        )]
        #[linkme(crate = $crate::openapi::registry::linkme)]
        static $ident: $crate::openapi::HandlerSpec = $spec;

        #[cfg(target_arch = "wasm32")]
        $crate::openapi::registry::inventory::submit! { $spec }
    };
}
