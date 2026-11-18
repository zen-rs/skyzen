//! Keeping a Durable Object's in-memory state resident while it has work.
//!
//! An idle Durable Object that can hibernate does so after a few seconds and discards its memory;
//! the next event rebuilds whatever the object keeps in fields from storage. That is the right
//! default — idle objects cost nothing — but an object whose in-memory state is expensive to
//! rebuild, and which knows it will be called again shortly, needs a way to say so.
//!
//! [`Residency::keep_resident`] is that statement. It returns a [`KeepResident`] guard, and the
//! object stays resident while any guard is alive; dropping the last one lets it hibernate again
//! on the platform's usual schedule. Guards count, so independent parts of an object can each
//! hold one without coordinating.

use core::future::{ready, Future};
use http_kit::http_error;
use skyzen_core::{Extractor, StatusCode};

/// Platform-specific residency control.
///
/// Implemented by the Cloudflare glue layer (or test mocks). The platform keeps the object
/// resident while the count of [`acquire`](Self::acquire) calls exceeds that of
/// [`release`](Self::release) calls; [`KeepResident`] is the only caller of either, so every
/// release pairs with an earlier acquire.
pub trait ResidencyInner: Send + Sync {
    /// Add one hold.
    fn acquire(&self);
    /// Remove one hold added by [`acquire`](Self::acquire).
    fn release(&self);
    /// Clone into a boxed trait object.
    fn clone_box(&self) -> Box<dyn ResidencyInner>;
}

/// The handle that keeps a Durable Object resident.
///
/// Available as an extractor in `fetch` and alarm handlers and via
/// [`DurableContext`](super::DurableContext) in `websocket` handlers.
pub struct Residency {
    inner: Box<dyn ResidencyInner>,
}

impl Clone for Residency {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_box(),
        }
    }
}

impl std::fmt::Debug for Residency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Residency").finish_non_exhaustive()
    }
}

impl Residency {
    /// Create from a platform-specific inner handle.
    #[must_use]
    pub fn new(inner: Box<dyn ResidencyInner>) -> Self {
        Self { inner }
    }

    /// Keep the object resident until the returned guard is dropped.
    ///
    /// On Cloudflare this makes the object ineligible for hibernation, so it is billed for
    /// wall-clock duration while held, idle or not. Hold it only while the object has work that
    /// will call it again; holding it forever turns an object that costs nothing when idle into
    /// one that is always billed.
    ///
    /// Residency is not durability: the platform still evicts an object on a deploy, a runtime
    /// restart, or after a longer period with no events, so state that must survive belongs in
    /// storage.
    pub fn keep_resident(&self) -> KeepResident {
        let inner = self.inner.clone_box();
        inner.acquire();
        KeepResident { inner }
    }
}

/// One hold on a Durable Object's residency, released when dropped.
#[must_use = "dropping the guard releases the hold at once"]
pub struct KeepResident {
    inner: Box<dyn ResidencyInner>,
}

impl std::fmt::Debug for KeepResident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeepResident").finish_non_exhaustive()
    }
}

impl Drop for KeepResident {
    fn drop(&mut self) {
        self.inner.release();
    }
}

http_error!(
    /// The `Residency` handle was not found in request extensions.
    pub ResidencyNotConfigured,
    StatusCode::INTERNAL_SERVER_ERROR,
    "Residency not configured. Ensure a ResidencyInner implementation is injected."
);

impl Extractor for Residency {
    type Error = ResidencyNotConfigured;

    // Reading the handle back out of the extensions is a synchronous clone, so the future is ready
    // on creation rather than an `async` block with nothing to await.
    fn extract(
        request: &mut http_kit::Request,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or(ResidencyNotConfigured::new()),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use http_kit::HttpError;
    use skyzen_core::Extractor;

    use super::{Residency, ResidencyInner};
    use crate::{Body, Request, StatusCode};

    /// Counts holds the way the platform does, and fails on a release with none outstanding.
    #[derive(Clone, Default)]
    struct CountingResidency {
        holds: Arc<AtomicUsize>,
    }

    impl ResidencyInner for CountingResidency {
        fn acquire(&self) {
            self.holds.fetch_add(1, Ordering::SeqCst);
        }

        fn release(&self) {
            let before = self.holds.fetch_sub(1, Ordering::SeqCst);
            assert!(before > 0, "released a hold that was never acquired");
        }

        fn clone_box(&self) -> Box<dyn ResidencyInner> {
            Box::new(self.clone())
        }
    }

    fn residency() -> (Residency, Arc<AtomicUsize>) {
        let inner = CountingResidency::default();
        let holds = Arc::clone(&inner.holds);
        (Residency::new(Box::new(inner)), holds)
    }

    #[test]
    fn each_guard_holds_until_it_is_dropped() {
        let (residency, holds) = residency();

        let first = residency.keep_resident();
        let second = residency.keep_resident();
        assert_eq!(holds.load(Ordering::SeqCst), 2);

        drop(first);
        assert_eq!(holds.load(Ordering::SeqCst), 1);
        drop(second);
        assert_eq!(holds.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_held_guard_outlives_the_handle_it_came_from() {
        let (residency, holds) = residency();

        let guard = residency.keep_resident();
        drop(residency);
        assert_eq!(holds.load(Ordering::SeqCst), 1);

        drop(guard);
        assert_eq!(holds.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn extractor_reads_injected_residency_from_request_extensions() {
        let (residency, holds) = residency();
        let mut request = Request::new(Body::empty());
        request.extensions_mut().insert(residency);

        let extracted = Residency::extract(&mut request).await.unwrap();
        let guard = extracted.keep_resident();

        assert_eq!(holds.load(Ordering::SeqCst), 1);
        drop(guard);
    }

    #[tokio::test]
    async fn extractor_errors_when_residency_is_missing() {
        let mut request = Request::new(Body::empty());

        let error = Residency::extract(&mut request).await.unwrap_err();

        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
