//! [`ServingThread`]: what a thread that builds a router hands the endpoint factory.

#[cfg(not(target_arch = "wasm32"))]
use http_kit_io::compio::{Compio, Ring};

/// The thread a router is being built on, as the host running that thread describes it.
///
/// Every host hands it to the [`EndpointFactory`](crate::EndpointFactory) on each thread that
/// builds a router, and `#[skyzen::main]` passes it to an entry function taking `&ServingThread`.
/// It carries what an endpoint can only get from the thread it serves on, so the endpoint takes
/// it when it is built rather than from a request:
///
/// - the thread's compio driver for its ring ([`compio`](Self::compio)), from which `Proxy::new`
///   (`proxy` feature) dials its upstream pool and the HTTP service clients `#[skyzen::main]`
///   wires are rebound to pools of the thread's own, and which carries the operator's live
///   zero-copy threshold and file table;
/// - on Linux, the region of registered buffers that ring registered (`--fixed-buffers`), into
///   which `StaticDir` (`static-files` feature) reads the files it keeps in memory.
///
/// A thread that runs no compio runtime — AWS Lambda's, a Workers isolate, an embedding host's
/// own executor — says so with [`without_compio`](Self::without_compio), and an endpoint that
/// needs a driver refuses it rather than guessing one.
///
/// It is the thread's: on a compio thread it is neither `Send` nor `Sync`.
///
/// ```rust,no_run
/// use skyzen::{
///     routing::{Route, Router},
///     runtime::ServingThread,
///     static_files::StaticDir,
/// };
///
/// #[skyzen::main]
/// fn main(thread: &ServingThread) -> Router {
///     Route::new((StaticDir::new("/assets", "./public", thread),)).build()
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ServingThread {
    #[cfg(not(target_arch = "wasm32"))]
    compio: Option<Compio<Ring>>,
}

impl ServingThread {
    /// A thread that runs a compio runtime, `ring` being the driver its ring registered (see
    /// [`Compio::register_fixed_region`]), with the region it registered where it has one.
    ///
    /// Built on that thread, inside its runtime.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub const fn on_ring(ring: Compio<Ring>) -> Self {
        Self { compio: Some(ring) }
    }

    /// A thread that runs no compio runtime: it has no driver to hand out and no region.
    #[must_use]
    pub const fn without_compio() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            compio: None,
        }
    }

    /// The thread's compio driver for its ring; `None` on a thread that runs no compio runtime.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub const fn compio(&self) -> Option<&Compio<Ring>> {
        self.compio.as_ref()
    }

    /// Where the HTTP service clients `#[skyzen::main]` wires pool on this thread: the shared
    /// settings of its driver, where it has one.
    #[cfg(all(not(target_arch = "wasm32"), feature = "rt"))]
    #[doc(hidden)]
    #[must_use]
    pub fn service_pools(&self) -> super::native::ServicePools {
        super::native::ServicePools::new(self.compio.as_ref().map(Compio::shared))
    }
}

#[cfg(all(test, feature = "rt"))]
mod tests {
    use super::ServingThread;
    use crate::runtime::native::IoOptions;

    /// A serving thread's clients pool on its driver's shared settings, so the zero-copy
    /// threshold and file table the operator changes are the ones their sockets read.
    #[test]
    fn a_serving_threads_pools_follow_the_operators_live_settings() {
        let operator = IoOptions::default().driver();
        let thread = crate::runtime::testing::block_on(async {
            ServingThread::on_ring(
                operator
                    .register_fixed_region()
                    .await
                    .expect("a ring asked for no region registers none"),
            )
        });
        let pools = thread
            .service_pools()
            .driver()
            .expect("a compio thread pools on its driver");
        operator.zero_copy().set_threshold(12_345);
        operator.file_table().set_slots(321);
        assert_eq!(pools.zero_copy().threshold(), 12_345);
        assert_eq!(pools.file_table().slots(), 321);
    }

    /// A thread without compio has nothing to pool on, and its clients keep zenwave's own driver
    /// thread.
    #[test]
    fn a_thread_without_compio_pools_nowhere_of_its_own() {
        assert!(ServingThread::without_compio()
            .service_pools()
            .driver()
            .is_none());
    }
}
