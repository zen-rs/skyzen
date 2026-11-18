//! Cloudflare Durable Object residency, over the Worker shim's controller.
//!
//! Cloudflare hibernates an idle Durable Object only when, among other conditions, no
//! `setTimeout`/`setInterval` callback is scheduled
//! (<https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/>). The
//! shim's controller schedules one interval timer while any hold is outstanding and clears it on
//! the last release, so a held object stays in memory and a released one hibernates on the
//! platform's usual schedule.

use skyzen::durable::{Residency, ResidencyInner};
use wasm_bindgen::{JsCast, JsValue};

use crate::ffi::ResidencyController;

/// Cloudflare residency handle over the shim's per-instance controller.
pub struct CfResidency {
    /// `None` only when the shim that constructed the instance passed no controller, which
    /// [`CfResidency::from_constructor`] reports; holds are then no-ops.
    controller: Option<ResidencyController>,
}

impl Clone for CfResidency {
    fn clone(&self) -> Self {
        Self {
            controller: self.controller.as_ref().map(|controller| {
                let js: &JsValue = controller.as_ref();
                js.clone().unchecked_into()
            }),
        }
    }
}

// SAFETY: Workers WASM executes on a single thread; JS handles are safe to mark Send/Sync.
unsafe impl Send for CfResidency {}
unsafe impl Sync for CfResidency {}

impl std::fmt::Debug for CfResidency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfResidency").finish_non_exhaustive()
    }
}

impl CfResidency {
    /// Wrap a residency controller.
    #[must_use]
    pub const fn new(controller: ResidencyController) -> Self {
        Self {
            controller: Some(controller),
        }
    }

    /// Wrap the value the shim passed as the constructor's residency argument.
    ///
    /// The shim `skyzen build` generates always passes one. A shim from an older `skyzen-cli`
    /// passes nothing: that is logged in a release build, and the object runs on without
    /// residency control.
    ///
    /// # Panics
    ///
    /// In a debug build, when the shim passed no controller.
    #[must_use]
    pub fn from_constructor(argument: JsValue) -> Self {
        const MISSING: &str = "the Worker shim passed this Durable Object no residency \
                               controller; rebuild the Worker with a skyzen-cli matching \
                               skyzen-cloudflare";
        debug_assert!(!argument.is_undefined(), "{MISSING}");
        if argument.is_undefined() {
            tracing::error!("{MISSING}");
            return Self { controller: None };
        }
        Self::new(argument.unchecked_into())
    }

    /// The portable handle handlers extract.
    #[must_use]
    pub fn residency(&self) -> Residency {
        Residency::new(Box::new(self.clone()))
    }
}

impl ResidencyInner for CfResidency {
    fn acquire(&self) {
        if let Some(controller) = &self.controller {
            controller.hold();
        }
    }

    fn release(&self) {
        if let Some(controller) = &self.controller {
            controller.release();
        }
    }

    fn clone_box(&self) -> Box<dyn ResidencyInner> {
        Box::new(self.clone())
    }
}
