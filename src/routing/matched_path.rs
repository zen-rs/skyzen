//! The route template the request matched.

use core::future::{ready, Future};
use std::sync::Arc;

use http_kit::{Request, StatusCode};
use skyzen_core::Extractor;

/// The route pattern that matched the request: `"/users/{id}"`, not `"/users/42"`.
///
/// The router inserts it next to [`Params`](crate::routing::Params) when a request matches,
/// so handlers can log or emit the low-cardinality template instead of the raw path:
///
/// ```rust
/// use skyzen::{Result, routing::MatchedPath};
///
/// async fn handler(route: MatchedPath) -> Result<String> {
///     Result::Ok(route.as_str().to_owned())
/// }
/// ```
///
/// [`Trace`](crate::middleware::Trace) reads it to fill the `http.route` span field.
/// Requests answered without a match — a custom `Route::fallback` — carry no
/// `MatchedPath`; extract `Option<MatchedPath>` there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedPath(Arc<str>);

impl MatchedPath {
    pub(crate) fn new(path: impl Into<Arc<str>>) -> Self {
        Self(path.into())
    }

    /// The matched template, e.g. `"/users/{id}"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for MatchedPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MatchedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

http_kit::http_error!(
    /// Raised when a handler extracts [`MatchedPath`] but no route matched.
    ///
    /// A handler is normally only reached *through* a match, so reaching this means the
    /// endpoint was served without one — a `Route::fallback` answering an unmatched
    /// request, or a bare endpoint in a test. Extract `Option<MatchedPath>` where absence
    /// is expected.
    pub NoMatchedPath,
    StatusCode::INTERNAL_SERVER_ERROR,
    "No route matched this request"
);

impl Extractor for MatchedPath {
    type Error = NoMatchedPath;

    // Reading the template back out of the extensions is a synchronous clone, so the
    // future is ready on creation rather than an `async` block with nothing to await.
    fn extract(request: &mut Request) -> impl Future<Output = Result<Self, Self::Error>> + Send {
        ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or_else(NoMatchedPath::new),
        )
    }
}
