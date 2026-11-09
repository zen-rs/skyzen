//! `Preconditions` — write-side `If-Match`/`If-None-Match`/`If-Unmodified-Since` evaluation.

use std::time::SystemTime;

use headers::{HeaderMapExt, IfMatch, IfNoneMatch, IfUnmodifiedSince};
use http_kit::{http_error, Request, StatusCode};
use skyzen_core::Extractor;

use crate::utils::conditional::EntityTag;

http_error!(
    /// `412 Precondition Failed` — `If-Match`/`If-Unmodified-Since` did not match.
    pub PreconditionFailed,
    StatusCode::PRECONDITION_FAILED,
    "the request's preconditions do not match the current resource"
);

http_error!(
    /// `428 Precondition Required` — the route demands `If-Match` and none was sent.
    pub PreconditionRequired,
    StatusCode::PRECONDITION_REQUIRED,
    "this route requires an If-Match precondition"
);

/// The request's write preconditions, for optimistic concurrency.
///
/// Extract once, then [`check`][Preconditions::check] the entity the write would replace —
/// a stale `If-Match` answers `412` before the mutation happens, so two writers cannot
/// silently clobber each other.
///
/// ```rust,ignore
/// async fn update(pre: Preconditions, Json(edit): Json<Edit>) -> Result<()> {
///     let current = load().await?;
///     pre.check(&EntityTag::strong(current.revision))?;
///     save(edit).await
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Preconditions {
    if_match: Option<IfMatch>,
    if_none_match: Option<IfNoneMatch>,
    if_unmodified_since: Option<IfUnmodifiedSince>,
}

impl Preconditions {
    /// `true` when the current `etag` satisfies `If-Match` *and* `If-None-Match`;
    /// otherwise `412`.
    ///
    /// `If-None-Match` matters on writes too: `If-None-Match: *` is create-only
    /// semantics — it fails as soon as the resource exists, which is exactly what
    /// `check`ing the current etag implies.
    ///
    /// `If-Unmodified-Since` is not consulted here — reach for
    /// [`check_modified`][Self::check_modified] when the entity has a timestamp to compare.
    /// With no precondition headers at all, the check passes —
    /// [`require`][Self::require] is the opt-in that makes them mandatory.
    ///
    /// # Errors
    /// [`PreconditionFailed`] when the request's validators reject the current state.
    pub fn check(&self, etag: &EntityTag) -> Result<(), PreconditionFailed> {
        if let Some(if_match) = &self.if_match {
            if !if_match.precondition_passes(etag.etag()) {
                return Err(PreconditionFailed::new());
            }
        }
        // A present `If-None-Match` whose precondition fails means the current state already
        // matches — `*` fails on any existing resource.
        if let Some(if_none_match) = &self.if_none_match {
            if !if_none_match.precondition_passes(etag.etag()) {
                return Err(PreconditionFailed::new());
            }
        }
        Ok(())
    }

    /// `check` plus an `If-Unmodified-Since` evaluation against `last_modified`, for
    /// entities whose optimistic-concurrency token is a timestamp rather than a tag.
    ///
    /// # Errors
    /// [`PreconditionFailed`] when the request's validators reject the current state.
    pub fn check_modified(
        &self,
        etag: &EntityTag,
        last_modified: SystemTime,
    ) -> Result<(), PreconditionFailed> {
        self.check(etag)?;
        if self.if_match.is_none() {
            if let Some(if_unmodified_since) = &self.if_unmodified_since {
                if !if_unmodified_since.precondition_passes(last_modified) {
                    return Err(PreconditionFailed::new());
                }
            }
        }
        Ok(())
    }

    /// Demand an `If-Match` header — answers `428` when a blind write arrives without one.
    /// Call before [`check`][Self::check] on routes that forbid unconditional overwrites.
    ///
    /// # Errors
    /// [`PreconditionRequired`] when the request carries no `If-Match`.
    pub const fn require(&self) -> Result<(), PreconditionRequired> {
        if self.if_match.is_none() {
            return Err(PreconditionRequired::new());
        }
        Ok(())
    }

    /// Whether the request carried `If-Match` at all.
    #[must_use]
    pub const fn has_if_match(&self) -> bool {
        self.if_match.is_some()
    }
}

impl Extractor for Preconditions {
    type Error = core::convert::Infallible;
    fn extract(
        request: &mut Request,
    ) -> impl core::future::Future<Output = Result<Self, Self::Error>> + Send {
        let preconditions = Self {
            if_match: request.headers().typed_get::<IfMatch>(),
            if_none_match: request.headers().typed_get::<IfNoneMatch>(),
            if_unmodified_since: request.headers().typed_get::<IfUnmodifiedSince>(),
        };
        std::future::ready(Ok(preconditions))
    }
}
